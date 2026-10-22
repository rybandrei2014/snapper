use crate::config::Config;
use crate::util;
use crate::WM_APP_REC_STOPPED;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub struct Recorder {
    stdin: Option<ChildStdin>,
    pub out_path: PathBuf,
}

impl Recorder {
    pub fn stop(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"q\n");
            let _ = s.flush();
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

    let spawn = |variant: &Variant, encoder: &str| -> Result<(Child, ChildStdin)> {
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

    let mut last_err = String::new();
    let mut started: Option<(Child, ChildStdin)> = None;
    'outer: for variant in variants(output_idx, cfg.fps, x, y, w, h) {
        for encoder in &encoders {
            let (mut child, stdin) = match spawn(&variant, encoder) {
                Ok(v) => v,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            std::thread::sleep(std::time::Duration::from_millis(400));
            match child.try_wait() {
                Ok(None) => {
                    started = Some((child, stdin));
                    break 'outer;
                }
                Ok(Some(status)) => {
                    last_err = format!("ffmpeg {encoder} exited: {status}");
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }
        }
    }

    let (child, stdin) = started.ok_or_else(|| anyhow!("no working capture pipeline: {last_err}"))?;

    let main_raw = main.0 as isize;
    std::thread::spawn(move || {
        let mut child = child;
        let code = child
            .wait()
            .map(|s| s.code().unwrap_or(0).max(0) as usize)
            .unwrap_or(1);
        let main = HWND(main_raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(main, WM_APP_REC_STOPPED, WPARAM(code), LPARAM(0));
        }
    });

    Ok(Recorder {
        stdin: Some(stdin),
        out_path,
    })
}
