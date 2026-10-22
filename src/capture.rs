use crate::audio::{self, MixFormat};
use crate::config::Config;
use crate::util;
use crate::WM_APP_REC_STOPPED;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub struct Recorder {
    stdin: Option<ChildStdin>,
    audio_stop: Option<Arc<AtomicBool>>,
    pub out_path: PathBuf,
}

impl Recorder {
    pub fn stop(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"q\n");
            let _ = s.flush();
        }
        if let Some(flag) = &self.audio_stop {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

struct Variant {
    input: String,
    vf: Option<String>,
}

fn variants(output_idx: i32, fps: u32, x: i32, y: i32, w: i32, h: i32) -> Vec<Variant> {
    let rate = format!("output_idx={output_idx}:framerate={fps}");
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

pub fn start(
    cfg: &Config,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    output_idx: i32,
    main: HWND,
    out_path: PathBuf,
) -> Result<Recorder> {
    let ffmpeg = util::find_ffmpeg(cfg)?;
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let w = w & !1;
    let h = h & !1;
    if w < 16 || h < 16 {
        return Err(anyhow!("selection too small"));
    }

    let encoders: Vec<&str> = match cfg.encoder.as_str() {
        "libx264" => vec!["libx264"],
        "h264_mf" => vec!["h264_mf"],
        _ => vec!["h264_mf", "libx264"],
    };

    let audio_mf = if cfg.audio {
        match audio::query_mix_format() {
            Ok(mf) => Some(mf),
            Err(e) => {
                eprintln!("[audio] unavailable, recording without sound: {e}");
                None
            }
        }
    } else {
        None
    };

    let spawn = |variant: &Variant,
                 encoder: &str,
                 audio_in: Option<(&MixFormat, &str)>|
     -> Result<(Child, ChildStdin)> {
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            variant.input.clone(),
        ];
        if let Some((mf, pipe)) = audio_in {
            args.extend([
                "-f".into(),
                mf.ffmpeg_fmt.into(),
                "-ar".into(),
                mf.rate.to_string(),
                "-ac".into(),
                mf.channels.to_string(),
                "-i".into(),
                pipe.into(),
            ]);
        }
        args.extend(["-map".into(), "0:v".into()]);
        if audio_in.is_some() {
            args.extend(["-map".into(), "1:a".into()]);
        }
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
        if let Some((_, _)) = audio_in {
            args.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{}K", cfg.audio_bitrate_kbps),
            ]);
        } else {
            args.push("-an".into());
        }
        args.extend([
            "-f".into(),
            "mp4".into(),
            "-y".into(),
            out_path.display().to_string(),
        ]);

        let mut child = Command::new(&ffmpeg)
            .args(&args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {ffmpeg:?}: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        Ok((child, stdin))
    };

    let pid = std::process::id();
    let mut pipe_seq = 0u32;
    let mut last_err = String::new();
    let mut started: Option<(Child, ChildStdin, Option<Arc<AtomicBool>>)> = None;

    'outer: for use_audio in [true, false] {
        if use_audio && audio_mf.is_none() {
            continue;
        }
        for variant in variants(output_idx, cfg.fps, x, y, w, h) {
            for encoder in &encoders {
                let audio_in = if use_audio {
                    pipe_seq += 1;
                    let pipe_name = format!(r"\\.\pipe\snapper-audio-{pid}-{pipe_seq}");
                    match audio::create_pipe(&pipe_name) {
                        Ok(handle) => Some((handle, pipe_name)),
                        Err(e) => {
                            last_err = e.to_string();
                            continue;
                        }
                    }
                } else {
                    None
                };

                let (mut child, stdin) = match &audio_in {
                    Some((_, name)) => {
                        match spawn(&variant, encoder, Some((audio_mf.as_ref().unwrap(), name))) {
                            Ok(v) => v,
                            Err(e) => {
                                last_err = e.to_string();
                                continue;
                            }
                        }
                    }
                    None => match spawn(&variant, encoder, None) {
                        Ok(v) => v,
                        Err(e) => {
                            last_err = e.to_string();
                            continue;
                        }
                    },
                };

                let audio_stop = if let Some((handle, _)) = &audio_in {
                    let flag = Arc::new(AtomicBool::new(false));
                    audio::start(*handle, flag.clone());
                    Some(flag)
                } else {
                    None
                };

                std::thread::sleep(std::time::Duration::from_millis(400));
                match child.try_wait() {
                    Ok(None) => {
                        started = Some((child, stdin, audio_stop));
                        break 'outer;
                    }
                    Ok(Some(status)) => {
                        last_err = format!("ffmpeg {encoder} exited: {status}");
                        if let Some(flag) = &audio_stop {
                            flag.store(true, Ordering::SeqCst);
                        }
                    }
                    Err(e) => {
                        last_err = e.to_string();
                    }
                }
            }
        }
        if audio_mf.is_none() {
            break; // no audio: single pass only
        }
    }

    let (child, stdin, audio_stop) =
        started.ok_or_else(|| anyhow!("no working capture pipeline: {last_err}"))?;

    let main_raw = main.0 as isize;
    let watcher_audio = audio_stop.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let code = child
            .wait()
            .map(|s| s.code().unwrap_or(0).max(0) as usize)
            .unwrap_or(1);
        if let Some(flag) = &watcher_audio {
            flag.store(true, Ordering::SeqCst);
        }
        let main = HWND(main_raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(main, WM_APP_REC_STOPPED, WPARAM(code), LPARAM(0));
        }
    });

    Ok(Recorder {
        stdin: Some(stdin),
        audio_stop,
        out_path,
    })
}
