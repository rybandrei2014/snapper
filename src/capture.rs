use crate::audio::{self, MixFormat, Source};
use crate::config::Config;
use crate::util;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JobObjectExtendedLimitInformation,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// One job for the whole process: every ffmpeg child dies with us,
/// even if we are force-killed (job handle intentionally leaked).
fn ffmpeg_job() -> HANDLE {
    static JOB: OnceLock<isize> = OnceLock::new();
    let raw = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, None).unwrap_or(INVALID_HANDLE_VALUE);
        if job != INVALID_HANDLE_VALUE {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
        }
        job.0 as isize
    });
    HANDLE(raw as *mut core::ffi::c_void)
}

pub struct Segment {
    child: Child,
    stdin: Option<ChildStdin>,
    audio_stops: Vec<Arc<AtomicBool>>,
}

impl Segment {
    /// Ask ffmpeg to finish this segment cleanly (writes moov atom).
    pub fn stop_signal(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"q\n");
            let _ = s.flush();
        }
        for flag in &self.audio_stops {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Non-blocking check whether the segment process has exited.
    pub fn try_done(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(0))),
            None => Ok(None),
        }
    }
}

#[derive(Clone, Copy)]
pub struct SegParams {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub output_idx: i32,
}

/// One recorded segment: video file plus optional raw audio files.
#[derive(Clone)]
pub struct SegEntry {
    pub video: PathBuf,
    pub audio: Vec<PathBuf>,
}

pub fn audio_path(video: &Path, source: &str) -> PathBuf {
    video.with_extension(format!("audio.{source}"))
}

struct Variant {
    input: String,
    vf: Option<String>,
}

fn variants(p: SegParams, fps: u32) -> Vec<Variant> {
    let rate = format!("output_idx={}:framerate={}", p.output_idx, fps);
    let (x, y, w, h) = (p.x, p.y, p.w, p.h);
    vec![
        // ffmpeg >= 6.1 with region options; frames come as D3D11 on recent builds
        Variant {
            input: format!(
                "ddagrab={rate}:draw_mouse=1:offset_x={x}:offset_y={y}:video_size={w}x{h}"
            ),
            vf: Some("hwdownload,format=bgra".into()),
        },
        // same region options, software frames (older 6.x builds)
        Variant {
            input: format!(
                "ddagrab={rate}:draw_mouse=1:offset_x={x}:offset_y={y}:video_size={w}x{h}"
            ),
            vf: None,
        },
        // no region options: grab whole monitor, crop in filtergraph
        Variant {
            input: format!("ddagrab={rate}"),
            vf: Some(format!("hwdownload,format=bgra,crop={w}:{h}:{x}:{y}")),
        },
        Variant {
            input: format!("ddagrab={rate}"),
            vf: Some(format!("crop={w}:{h}:{x}:{y}")),
        },
    ]
}

pub fn start_segment(
    cfg: &Config,
    p: SegParams,
    seg_path: &Path,
    encoder_override: Option<&str>,
) -> Result<(Segment, SegEntry, Option<MixFormat>)> {
    let ffmpeg = util::find_ffmpeg(cfg)?;
    if let Some(dir) = seg_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    // H.264 requires even dimensions
    let p = SegParams {
        x: p.x,
        y: p.y,
        w: p.w & !1,
        h: p.h & !1,
        output_idx: p.output_idx,
    };
    if p.w < 16 || p.h < 16 {
        return Err(anyhow!("selection too small"));
    }

    let encoders: Vec<&str> = match encoder_override {
        Some(e) => vec![e],
        None => match cfg.encoder.as_str() {
            "libx264" => vec!["libx264"],
            "h264_mf" => vec!["h264_mf"],
            _ => vec!["h264_mf", "libx264"],
        },
    };

    // audio: capture to raw files (no pipes); mixed/muxed at finalize
    let audio_specs: Vec<(Source, PathBuf, MixFormat)> = {
        let mut v = Vec::new();
        if cfg.audio {
            match audio::query_mix_format(Source::Loopback) {
                Ok(mf) => v.push((Source::Loopback, audio_path(seg_path, "sys"), mf)),
                Err(e) => eprintln!("[audio] unavailable, recording without sound: {e}"),
            }
        }
        if cfg.mic && !v.is_empty() {
            match audio::query_mix_format(Source::Mic) {
                Ok(mf) => v.push((Source::Mic, audio_path(seg_path, "mic"), mf)),
                Err(e) => eprintln!("[audio] mic unavailable: {e}"),
            }
        }
        v
    };
    let primary_fmt = audio_specs.first().map(|(_, _, mf)| *mf);

    let spawn = |variant: &Variant, encoder: &str, vi: usize| -> Result<(Child, ChildStdin)> {
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            variant.input.clone(),
        ];
        if let Some(vf) = &variant.vf {
            args.extend(["-vf".into(), vf.clone()]);
        }
        if encoder == "libx264" {
            args.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "veryfast".into(),
                "-crf".into(),
                "23".into(),
                "-pix_fmt".into(),
                "yuv420p".into(),
            ]);
        } else {
            args.extend([
                "-c:v".into(),
                "h264_mf".into(),
                "-b:v".into(),
                format!("{}K", cfg.bitrate_kbps),
                "-pix_fmt".into(),
                "nv12".into(),
            ]);
        }
        args.extend([
            "-an".into(),
            "-f".into(),
            "mp4".into(),
            "-y".into(),
            seg_path.display().to_string(),
        ]);

        let log_path = seg_path.with_extension(format!("v{vi}.{encoder}.log"));
        let log_file = std::fs::File::create(&log_path).ok();
        let mut child = Command::new(&ffmpeg)
            .args(&args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(match log_file {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {ffmpeg:?}: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        let job = ffmpeg_job();
        if job != INVALID_HANDLE_VALUE {
            unsafe {
                let ph = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child.id())
                    .unwrap_or(INVALID_HANDLE_VALUE);
                if ph != INVALID_HANDLE_VALUE {
                    let _ = AssignProcessToJobObject(job, ph);
                    let _ = CloseHandle(ph);
                }
            }
        }
        Ok((child, stdin))
    };

    let mut last_err = String::new();
    let mut started: Option<(Child, ChildStdin)> = None;

    for (vi, variant) in variants(p, cfg.fps).iter().enumerate() {
        for encoder in &encoders {
            let (mut child, stdin) = match spawn(variant, encoder, vi) {
                Ok(v) => v,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };

            // health window: bad variants (bad options, filter format errors)
            // exit within ~1s. Do NOT gate on file size: static screens
            // encode to near-zero bitrate and flush the mp4 only much later.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
            let mut alive = true;
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if let Some(st) = child.try_wait()? {
                    alive = false;
                    last_err = format!("ffmpeg {encoder} exited: {:?}", st.code());
                    break;
                }
            }
            if alive {
                started = Some((child, stdin));
                break;
            }
            let _ = child.kill();
        }
        if started.is_some() {
            break;
        }
    }

    let (child, stdin) = started.ok_or_else(|| anyhow!("no working capture: {last_err}"))?;

    // start audio capture threads writing raw files for this segment
    let mut audio_stops = Vec::new();
    let mut audio_files = Vec::new();
    for (source, path, _mf) in &audio_specs {
        let flag = Arc::new(AtomicBool::new(false));
        audio::start(*source, path.clone(), flag.clone());
        audio_stops.push(flag);
        audio_files.push(path.clone());
    }

    Ok((
        Segment {
            child,
            stdin: Some(stdin),
            audio_stops,
        },
        SegEntry {
            video: seg_path.to_path_buf(),
            audio: audio_files,
        },
        primary_fmt,
    ))
}

/// Grab a single still frame of the region to a PNG (Snipping-Tool style).
pub fn grab_frame(cfg: &Config, p: SegParams, out: &Path) -> Result<()> {
    let ffmpeg = util::find_ffmpeg(cfg)?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut last_err = String::new();
    for variant in variants(p, cfg.fps) {
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            variant.input.clone(),
        ];
        if let Some(vf) = &variant.vf {
            args.extend(["-vf".into(), vf.clone()]);
        }
        args.extend([
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "image2".into(),
            "-y".into(),
            out.display().to_string(),
        ]);
        let status = Command::new(&ffmpeg)
            .args(&args)
            .creation_flags(0x0800_0000)
            .status()
            .map_err(|e| anyhow!("failed to spawn {ffmpeg:?}: {e}"))?;
        if status.success() && std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(());
        }
        last_err = format!("ffmpeg exited {status}");
    }
    Err(anyhow!("no working frame grab: {last_err}"))
}

/// Join finished segments into the final file and remove intermediates.
pub fn finalize(
    cfg: &Config,
    base: &Path,
    segments: &[SegEntry],
    audio_fmt: Option<MixFormat>,
) -> Result<()> {
    let good: Vec<&SegEntry> = segments
        .iter()
        .filter(|s| std::fs::metadata(&s.video).map(|m| m.len() > 1024).unwrap_or(false))
        .collect();
    for bad in segments
        .iter()
        .filter(|s| !good.iter().any(|g| g.video == s.video))
    {
        cleanup(bad);
    }
    if good.is_empty() {
        return Err(anyhow!("no valid segments"));
    }

    // 1. video: single segment → rename, else concat demuxer (stream copy)
    let video_only = base.with_extension("video.tmp.mp4");
    if good.len() == 1 {
        std::fs::rename(&good[0].video, &video_only)?;
    } else {
        let list = base.with_extension("concat.txt");
        let mut text = String::new();
        for s in &good {
            text.push_str(&format!("file '{}'\n", s.video.display()));
        }
        std::fs::write(&list, text)?;
        let ffmpeg = util::find_ffmpeg(cfg)?;
        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                &list.display().to_string(),
                "-c",
                "copy",
                "-y",
                &video_only.display().to_string(),
            ])
            .creation_flags(0x0800_0000)
            .status()?;
        let _ = std::fs::remove_file(&list);
        if !status.success() {
            return Err(anyhow!("concat failed: {status}"));
        }
    }

    // 2. audio: raw files concatenate trivially; loopback + mic mixed via amix
    let ffmpeg = util::find_ffmpeg(cfg)?;
    let have_audio: Vec<&PathBuf> = good
        .iter()
        .flat_map(|s| s.audio.iter())
        .filter(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false))
        .collect();

    if let (Some(mf), true) = (audio_fmt, !have_audio.is_empty()) {
        // concatenate each source's raw files across segments
        let sys_raw = base.with_extension("sys.raw");
        let mic_raw = base.with_extension("mic.raw");
        let mut sys = std::fs::File::create(&sys_raw)?;
        let mut mic = std::fs::File::create(&mic_raw)?;
        let mut mic_created = false;
        for s in &good {
            for p in &s.audio {
                let is_mic = p.extension().map(|e| e == "mic").unwrap_or(false);
                let target: &mut std::fs::File = if is_mic {
                    mic_created = true;
                    &mut mic
                } else {
                    &mut sys
                };
                if let Ok(data) = std::fs::read(p) {
                    let _ = target.write_all(&data);
                }
            }
        }
        drop(sys);
        drop(mic);

        let fmt_args = |raw: &Path| -> Vec<String> {
            vec![
                "-f".into(),
                mf.ffmpeg_fmt.into(),
                "-ar".into(),
                mf.rate.to_string(),
                "-ac".into(),
                mf.channels.to_string(),
                "-i".into(),
                raw.display().to_string(),
            ]
        };
        let mut args: Vec<String> =
            vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        args.extend(["-i".into(), video_only.display().to_string()]);
        args.extend(fmt_args(&sys_raw));
        if mic_created {
            args.extend(fmt_args(&mic_raw));
            args.extend([
                "-filter_complex".into(),
                "[1:a][2:a]amix=inputs=2:duration=longest[aout]".into(),
                "-map".into(),
                "0:v".into(),
                "-map".into(),
                "[aout]".into(),
            ]);
        } else {
            args.extend(["-map".into(), "0:v".into(), "-map".into(), "1:a".into()]);
        }
        args.extend([
            "-c:v".into(),
            "copy".into(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}K", cfg.audio_bitrate_kbps),
            "-shortest".into(),
            "-y".into(),
            base.display().to_string(),
        ]);
        let status = Command::new(&ffmpeg)
            .args(&args)
            .creation_flags(0x0800_0000)
            .status()?;
        let _ = std::fs::remove_file(&sys_raw);
        let _ = std::fs::remove_file(&mic_raw);
        if !status.success() {
            // fall back to silent video rather than losing the recording
            eprintln!("[finalize] audio mux failed ({status}); keeping video");
            std::fs::rename(&video_only, base)?;
        } else {
            let _ = std::fs::remove_file(&video_only);
        }
    } else {
        std::fs::rename(&video_only, base)?;
    }

    for s in segments {
        cleanup(s);
    }
    Ok(())
}

fn cleanup(s: &SegEntry) {
    let _ = std::fs::remove_file(&s.video);
    for a in &s.audio {
        let _ = std::fs::remove_file(a);
    }
    if let (Some(dir), Some(name)) = (s.video.parent(), s.video.file_stem()) {
        let prefix = format!("{}.", name.to_string_lossy());
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&prefix) && n.ends_with(".log") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}
