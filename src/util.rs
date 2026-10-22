use crate::config::Config;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, HDC, MONITORINFO,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn monitors() -> Vec<RECT> {
    unsafe {
        let boxed = Box::into_raw(Box::new(Vec::<RECT>::new()));
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_monitors_cb),
            LPARAM(boxed as *mut _ as isize),
        );
        *Box::from_raw(boxed)
    }
}

unsafe extern "system" fn enum_monitors_cb(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<RECT>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
        list.push(mi.rcMonitor);
    }
    true.into()
}

pub fn overlap(a: RECT, b: RECT) -> i32 {
    let x1 = a.left.max(b.left);
    let y1 = a.top.max(b.top);
    let x2 = a.right.min(b.right);
    let y2 = a.bottom.min(b.bottom);
    (x2 - x1).max(0) * (y2 - y1).max(0)
}

pub fn pick_monitor(rect: RECT, mons: &[RECT]) -> usize {
    let mut best = 0usize;
    let mut best_area = -1i32;
    for (i, m) in mons.iter().enumerate() {
        let area = overlap(rect, *m);
        if area > best_area {
            best_area = area;
            best = i;
        }
    }
    best
}

pub fn clamp_to(rect: RECT, m: RECT) -> RECT {
    RECT {
        left: rect.left.max(m.left),
        top: rect.top.max(m.top),
        right: rect.right.min(m.right),
        bottom: rect.bottom.min(m.bottom),
    }
}

pub fn find_ffmpeg(cfg: &Config) -> Result<PathBuf> {
    if let Some(p) = &cfg.ffmpeg_path {
        if p.exists() {
            return Ok(p.clone());
        }
        return Err(anyhow!("configured ffmpeg_path not found: {p:?}"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("ffmpeg.exe");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    Ok(PathBuf::from("ffmpeg.exe"))
}
