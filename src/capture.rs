use crate::audio::{self, MixFormat, Source};
use crate::config::Config;
use crate::util;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

static PIPE_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct Segment {
    child: Child,
    stdin: Option<ChildStdin>,
    audio_stops: Vec<Arc<AtomicBool>>,
    audio_pipes: Vec<isize>,
    start: std::time::Instant,
}

impl Segment {
    /// Seconds of wall-clock since this capture started. Pause/resume points
    /// are marked on this clock and mapped onto the file timeline at finalize.
    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Ask ffmpeg to finish this segment cleanly (writes moov atom).
    pub fn stop_signal(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"q\n");
            let _ = s.flush();
        }
        self.shutdown_audio();
    }

    fn shutdown_audio(&mut self) {
        for flag in &self.audio_stops {
            flag.store(true, Ordering::SeqCst);
        }
        // Closing the server pipe unblocks any capture thread still in
        // ConnectNamedPipe and makes in-flight writes fail.
        for h in self.audio_pipes.drain(..) {
            audio::close_pipe(h);
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

impl Drop for Segment {
    fn drop(&mut self) {
        self.shutdown_audio();
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

#[allow(clippy::too_many_arguments)]
fn build_args(
    variant: &Variant,
    encoder: &str,
    audio: &[(Source, MixFormat)],
    pipes: &[String],
    seg_path: &Path,
    cfg: &Config,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        variant.input.clone(),
    ];
    // audio inputs (each fed live from a named pipe)
    for ((_, mf), name) in audio.iter().zip(pipes.iter()) {
        args.extend([
            "-f".into(),
            mf.ffmpeg_fmt.into(),
            "-ar".into(),
            mf.rate.to_string(),
            "-ac".into(),
            mf.channels.to_string(),
            "-i".into(),
            name.clone(),
        ]);
    }
    // -vf is an output-side filter: must come after ALL inputs
    if let Some(vf) = &variant.vf {
        args.extend(["-vf".into(), vf.clone()]);
    }
    // stream mapping
    match audio.len() {
        0 => args.push("-an".into()),
        1 => args.extend(["-map".into(), "0:v".into(), "-map".into(), "1:a".into()]),
        _ => args.extend([
            "-filter_complex".into(),
            "[1:a][2:a]amix=inputs=2:duration=longest[a]".into(),
            "-map".into(),
            "0:v".into(),
            "-map".into(),
            "[a]".into(),
        ]),
    }
    // video encoder
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
    if !audio.is_empty() {
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}K", cfg.audio_bitrate_kbps),
        ]);
    }
    args.extend([
        "-f".into(),
        "mp4".into(),
        "-y".into(),
        seg_path.display().to_string(),
    ]);
    args
}

pub fn start_segment(
    cfg: &Config,
    p: SegParams,
    seg_path: &Path,
    encoder_override: Option<&str>,
) -> Result<Segment> {
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

    // audio sources + formats (queried once; pipes created per attempt)
    let audio_specs: Vec<(Source, MixFormat)> = {
        let mut v = Vec::new();
        if cfg.audio {
            match audio::query_mix_format(Source::Loopback) {
                Ok(mf) => v.push((Source::Loopback, mf)),
                Err(e) => eprintln!("[audio] unavailable, recording without sound: {e}"),
            }
        }
        if cfg.mic && !v.is_empty() {
            match audio::query_mix_format(Source::Mic) {
                Ok(mf) => v.push((Source::Mic, mf)),
                Err(e) => eprintln!("[audio] mic unavailable: {e}"),
            }
        }
        v
    };

    let mut last_err = String::new();
    let mut started: Option<(Child, ChildStdin, Vec<Arc<AtomicBool>>, Vec<isize>, std::time::Instant)> =
        None;

    for (vi, variant) in variants(p, cfg.fps).iter().enumerate() {
        for encoder in &encoders {
            // fresh pipes for this attempt
            let mut pipe_handles: Vec<isize> = Vec::new();
            let mut pipe_names: Vec<String> = Vec::new();
            let mut pipes_ok = true;
            for _ in 0..audio_specs.len() {
                let name = format!(
                    r"\\.\pipe\snapper-{}-{}",
                    std::process::id(),
                    PIPE_SEQ.fetch_add(1, Ordering::Relaxed)
                );
                match audio::create_pipe(&name) {
                    Ok(h) => {
                        pipe_handles.push(h);
                        pipe_names.push(name);
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        pipes_ok = false;
                        break;
                    }
                }
            }
            if !pipes_ok {
                for h in pipe_handles {
                    audio::close_pipe(h);
                }
                continue;
            }

            // start audio threads now so WASAPI init overlaps ffmpeg startup;
            // each blocks in ConnectNamedPipe until ffmpeg opens its input,
            // then streams PCM in real time (single clock with video).
            let mut audio_stops = Vec::new();
            for (si, (src, _)) in audio_specs.iter().enumerate() {
                let flag = Arc::new(AtomicBool::new(false));
                audio::start(*src, pipe_handles[si], flag.clone());
                audio_stops.push(flag);
            }

            let args = build_args(variant, encoder, &audio_specs, &pipe_names, seg_path, cfg);
            let log_path = seg_path.with_extension(format!("v{vi}.{encoder}.log"));
            let log_file = std::fs::File::create(&log_path).ok();
            let spawn_res = Command::new(&ffmpeg)
                .args(&args)
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(match log_file {
                    Some(f) => Stdio::from(f),
                    None => Stdio::null(),
                })
                .spawn()
                .map_err(|e| anyhow!("failed to spawn {ffmpeg:?}: {e}"));
            let mut child = match spawn_res {
                Ok(c) => c,
                Err(e) => {
                    last_err = e.to_string();
                    for h in pipe_handles {
                        audio::close_pipe(h);
                    }
                    continue;
                }
            };
            let stdin = match child.stdin.take() {
                Some(s) => s,
                None => {
                    last_err = "no stdin pipe".into();
                    let _ = child.kill();
                    for h in pipe_handles {
                        audio::close_pipe(h);
                    }
                    continue;
                }
            };
            // wall-clock origin = spawn instant, so pause marks align with the
            // video timeline (first frame lags this by a small constant)
            let t0 = std::time::Instant::now();
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

            // short health window: bad variants (bad options, filter format
            // errors) exit within ~100 ms. Keep it short so Resume feels
            // instant and the UI thread isn't blocked; the border + control
            // bar are already on screen before this runs.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            let mut alive = true;
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some(st) = child.try_wait()? {
                    alive = false;
                    last_err = format!("ffmpeg {encoder} exited: {:?}", st.code());
                    break;
                }
            }
            if alive {
                started = Some((child, stdin, audio_stops, pipe_handles, t0));
                break;
            }

            // attempt failed: kill ffmpeg, stop+unblock audio threads
            let _ = child.kill();
            for f in &audio_stops {
                f.store(true, Ordering::SeqCst);
            }
            for h in &pipe_handles {
                audio::close_pipe(*h);
            }
        }
        if started.is_some() {
            break;
        }
    }

    let (child, stdin, audio_stops, audio_pipes, t0) =
        started.ok_or_else(|| anyhow!("no working capture: {last_err}"))?;

    Ok(Segment {
        child,
        stdin: Some(stdin),
        audio_stops,
        audio_pipes,
        start: t0,
    })
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

/// Produce the final file from one continuous recording by cutting out the
/// paused spans. `paused_wall` are (start,end) seconds on the wall-clock since
/// capture started; `total_wall` is the wall-clock elapsed at stop. The wall
/// clock is mapped onto the file timeline by calibrating the startup latency.
pub fn finalize(
    cfg: &Config,
    base: &Path,
    raw: &Path,
    paused_wall: &[(f64, f64)],
    total_wall: f64,
) -> Result<()> {
    if std::fs::metadata(raw).map(|m| m.len() <= 1024).unwrap_or(true) {
        cleanup(raw);
        return Err(anyhow!("no valid recording"));
    }

    if paused_wall.is_empty() {
        std::fs::rename(raw, base)?;
        return Ok(());
    }

    let total_out = ffprobe_duration(cfg, raw)?;
    // ffmpeg's first frame lags spawn by a constant `init`; map wall → output.
    let init = (total_wall - total_out).max(0.0);
    let spans: Vec<(f64, f64)> = paused_wall
        .iter()
        .map(|(s, e)| ((s - init).max(0.0), (e - init).min(total_out).max(0.0)))
        .filter(|(s, e)| e > s)
        .collect();

    // merge + clamp paused spans, then take the complement = kept spans
    let mut spans = spans;
    spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in spans {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    let mut kept: Vec<(f64, f64)> = Vec::new();
    let mut cur = 0.0;
    for (s, e) in &merged {
        if *s > cur {
            kept.push((cur, *s));
        }
        cur = cur.max(*e);
    }
    if cur < total_out {
        kept.push((cur, total_out));
    }
    kept.retain(|(s, e)| e - s > 0.05);
    if kept.is_empty() {
        cleanup(raw);
        return Err(anyhow!("nothing left after trim"));
    }

    let with_audio = cfg.audio;
    let mut fc = String::new();
    let mut inputs = String::new();
    for (i, (s, e)) in kept.iter().enumerate() {
        fc.push_str(&format!(
            "[0:v]trim=start={s:.3}:end={e:.3},setpts=PTS-STARTPTS[v{i}];"
        ));
        if with_audio {
            fc.push_str(&format!(
                "[0:a]atrim=start={s:.3}:end={e:.3},asetpts=PTS-STARTPTS[a{i}];"
            ));
            inputs.push_str(&format!("[v{i}][a{i}]"));
        } else {
            inputs.push_str(&format!("[v{i}]"));
        }
    }
    fc.push_str(&format!(
        "{}concat=n={}:v=1:a={}[outv][outa]",
        inputs,
        kept.len(),
        if with_audio { 1 } else { 0 }
    ));

    let ffmpeg = util::find_ffmpeg(cfg)?;
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        raw.display().to_string(),
        "-filter_complex".into(),
        fc,
        "-map".into(),
        "[outv]".into(),
    ];
    if with_audio {
        args.extend(["-map".into(), "[outa]".into()]);
    }
    args.extend([
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "20".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ]);
    if with_audio {
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}K", cfg.audio_bitrate_kbps),
        ]);
    }
    args.extend(["-y".into(), base.display().to_string()]);

    let status = Command::new(&ffmpeg)
        .args(&args)
        .creation_flags(0x0800_0000)
        .status()?;
    if !status.success() {
        cleanup(raw);
        return Err(anyhow!("trim failed: {status}"));
    }
    cleanup(raw);
    Ok(())
}

fn ffprobe_duration(cfg: &Config, path: &Path) -> Result<f64> {
    let ffprobe = util::find_ffmpeg(cfg)?.with_file_name("ffprobe.exe");
    let out = Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &path.display().to_string(),
        ])
        .creation_flags(0x0800_0000)
        .output()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse::<f64>()
        .map_err(|_| anyhow!("ffprobe could not read duration of {path:?}"))
}

fn cleanup(raw: &Path) {
    let _ = std::fs::remove_file(raw);
    if let (Some(dir), Some(name)) = (raw.parent(), raw.file_stem()) {
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
