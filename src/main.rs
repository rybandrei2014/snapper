// Release: GUI subsystem so launching from Explorer/Shell shows no console
// window — the app is tray-only. Debug keeps the console for e2e stderr.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod config;
mod overlay;
mod util;

use anyhow::{Context, Result};
use std::mem::size_of;
use std::path::PathBuf;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    HWND, LPARAM, LRESULT, WPARAM, ERROR_ALREADY_EXISTS, GetLastError,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_P, VK_R,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub use overlay::WM_APP_SELECTION;
const HOTKEY_REC: i32 = 1;
const HOTKEY_PAUSE: i32 = 2;

#[derive(PartialEq, Clone, Copy, Debug)]
enum Mode {
    Idle,
    Selecting,
    Recording,
    Pausing,
    Paused,
    Stopping,
}

struct Recording {
    params: capture::SegParams,
    base: PathBuf,
    segments: Vec<capture::SegEntry>,
    audio_fmt: Option<audio::MixFormat>,
    seq: u32,
    seg: Option<capture::Segment>,
    retried: bool,
}

struct App {
    cfg: config::Config,
    mode: Mode,
    recording: Option<Recording>,
    last_out: Option<PathBuf>,
}

struct MenuItems {
    start: MenuItem,
    pause: MenuItem,
    resume: MenuItem,
    stop: MenuItem,
    open: MenuItem,
    quit: MenuItem,
}

unsafe extern "system" fn main_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn make_tray_icon() -> Icon {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if (dx * dx + dy * dy).sqrt() <= 13.5 {
                let i = (y * size + x) * 4;
                rgba[i] = 225;
                rgba[i + 1] = 40;
                rgba[i + 2] = 40;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, size as u32, size as u32).expect("icon")
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mtx = CreateMutexW(None, true, w!("Snapper.SingleInstance")).context("mutex")?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            eprintln!("Snapper is already running (check the system tray)");
            std::process::exit(0);
        }
        let _ = mtx;
    }

    let cfg = config::Config::load()?;
    let mut app = App {
        cfg,
        mode: Mode::Idle,
        recording: None,
        last_out: None,
    };

    let hinst = unsafe { GetModuleHandleW(None) }?;
    let cls = util::wide("SnapperMain");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(main_wndproc),
        hInstance: hinst.into(),
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow::anyhow!("RegisterClassExW main failed"));
        }
    }
    let main = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        )
    }?;
    unsafe { RegisterHotKey(main, HOTKEY_REC, MOD_WIN | MOD_SHIFT, VK_R.0 as u32) }
        .context("hotkey Win+Shift+R is taken")?;
    // pause hotkey: try candidates, first that registers wins (vendor tools squat keys)
    let pause_candidates = [
        (MOD_WIN | MOD_ALT, "Win+Alt+P"),
        (MOD_CONTROL | MOD_ALT, "Ctrl+Alt+P"),
    ];
    let mut pause_label = None;
    for (mods, label) in pause_candidates {
        if unsafe { RegisterHotKey(main, HOTKEY_PAUSE, mods, VK_P.0 as u32) }.is_ok() {
            pause_label = Some(label);
            break;
        }
    }
    let pause_label = pause_label.unwrap_or("Win+Shift+P");
    eprintln!("[app] pause hotkey: {pause_label}");
    overlay::register_class()?;

    let menu = Menu::new();
    let items = MenuItems {
        start: MenuItem::new("Start recording  (Win+Shift+R)", true, None),
        pause: MenuItem::new(&format!("Pause  ({pause_label})"), false, None),
        resume: MenuItem::new(&format!("Resume ({pause_label})"), false, None),
        stop: MenuItem::new("Stop", false, None),
        open: MenuItem::new("Open output folder", true, None),
        quit: MenuItem::new("Quit", true, None),
    };
    let sep = PredefinedMenuItem::separator();
    let _ = menu.append(&items.start);
    let _ = menu.append(&items.pause);
    let _ = menu.append(&items.resume);
    let _ = menu.append(&items.stop);
    let _ = menu.append(&sep);
    let _ = menu.append(&items.open);
    let _ = menu.append(&items.quit);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Snapper — Win+Shift+R to record")
        .with_icon(make_tray_icon())
        .build()
        .map_err(|e| anyhow::anyhow!("tray: {e:?}"))?;

    let mut msg = MSG::default();
    loop {
        let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
        if got.as_bool() {
            match msg.message {
                WM_QUIT => break,
                WM_HOTKEY => match msg.wParam.0 as i32 {
                    HOTKEY_REC => toggle_rec(&mut app, main, &tray, &items),
                    HOTKEY_PAUSE => toggle_pause(&mut app, &tray, &items),
                    _ => {}
                },
                WM_APP_SELECTION => on_selection(&mut app, msg.wParam.0, &tray, &items),
                _ => {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        } else {
            poll_segment(&mut app, &tray, &items);
            if let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id() == items.start.id() {
                    toggle_rec(&mut app, main, &tray, &items);
                } else if ev.id() == items.pause.id() || ev.id() == items.resume.id() {
                    toggle_pause(&mut app, &tray, &items);
                } else if ev.id() == items.stop.id() {
                    stop_rec(&mut app, &tray, &items);
                } else if ev.id() == items.open.id() {
                    open_folder(&app);
                } else if ev.id() == items.quit.id() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // quitting while recording: finish current segment and salvage the file
    if let Some(rec) = app.recording.as_mut() {
        if let Some(seg) = rec.seg.as_mut() {
            seg.stop_signal();
            std::thread::sleep(Duration::from_millis(500));
        }
        rec.seg = None;
        let _ = capture::finalize(&app.cfg, &rec.base, &rec.segments, rec.audio_fmt);
    }
    Ok(())
}

fn update_menu(mode: Mode, items: &MenuItems) {
    let _ = items.start.set_enabled(mode == Mode::Idle);
    let _ = items.pause.set_enabled(mode == Mode::Recording);
    let _ = items.resume.set_enabled(mode == Mode::Paused);
    let _ = items
        .stop
        .set_enabled(matches!(mode, Mode::Recording | Mode::Pausing | Mode::Paused | Mode::Stopping));
}

fn toggle_rec(app: &mut App, main: HWND, tray: &TrayIcon, items: &MenuItems) {
    match app.mode {
        Mode::Idle => {
            app.mode = Mode::Selecting;
            update_menu(app.mode, items);
            if let Err(e) = overlay::show(main) {
                eprintln!("overlay: {e}");
                app.mode = Mode::Idle;
                update_menu(app.mode, items);
            }
        }
        Mode::Recording => stop_rec(app, tray, items),
        Mode::Paused => {
            // stop while paused: finalize what we have
            app.mode = Mode::Stopping;
            update_menu(app.mode, items);
            finalize(app, tray);
        }
        _ => {}
    }
}

fn toggle_pause(app: &mut App, tray: &TrayIcon, items: &MenuItems) {
    match app.mode {
        Mode::Recording => {
            if let Some(rec) = app.recording.as_mut() {
                if let Some(seg) = rec.seg.as_mut() {
                    app.mode = Mode::Pausing;
                    update_menu(app.mode, items);
                    seg.stop_signal();
                    let _ = tray.set_tooltip(Some("Snapper — pausing…"));
                }
            }
        }
        Mode::Paused => {
            let rec = match app.recording.as_mut() {
                Some(r) => r,
                None => return,
            };
            rec.seq += 1;
            let seg_path = segment_path(&rec.base, rec.seq);
            let enc = if rec.retried { Some("libx264") } else { None };
            match capture::start_segment(&app.cfg, rec.params, &seg_path, enc) {
                Ok((seg, entry, fmt)) => {
                    rec.segments.push(entry);
                    if rec.audio_fmt.is_none() {
                        rec.audio_fmt = fmt;
                    }
                    rec.seg = Some(seg);
                    app.mode = Mode::Recording;
                    update_menu(app.mode, items);
                    let _ = tray.set_tooltip(Some("Snapper — recording"));
                }
                Err(e) => {
                    let _ = tray.set_tooltip(Some(&format!("Snapper — resume failed: {e}")));
                }
            }
        }
        _ => {}
    }
}

fn stop_rec(app: &mut App, tray: &TrayIcon, items: &MenuItems) {
    if app.mode != Mode::Recording {
        return;
    }
    app.mode = Mode::Stopping;
    update_menu(app.mode, items);
    if let Some(rec) = app.recording.as_mut() {
        if let Some(seg) = rec.seg.as_mut() {
            seg.stop_signal();
            let _ = tray.set_tooltip(Some("Snapper — stopping…"));
        } else {
            finalize(app, tray);
        }
    }
}

fn segment_path(base: &PathBuf, seq: u32) -> PathBuf {
    base.with_extension(format!("seg{seq:03}.mp4"))
}

fn on_selection(app: &mut App, ok: usize, tray: &TrayIcon, items: &MenuItems) {
    app.mode = Mode::Idle;
    update_menu(app.mode, items);
    if ok == 0 {
        return;
    }
    let Some(rect) = overlay::take_result() else {
        return;
    };
    let mons = util::monitors();
    if mons.is_empty() {
        return;
    }
    let idx = app
        .cfg
        .output_idx_override
        .unwrap_or_else(|| util::pick_monitor(rect, &mons) as i32);
    let mon = mons.get(idx as usize).copied().unwrap_or(mons[0]);
    let rect = util::clamp_to(rect, mon);

    let name = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let base = app.cfg.output_dir.join(format!("{name}.mp4"));
    let params = capture::SegParams {
        x: rect.left - mon.left,
        y: rect.top - mon.top,
        w: rect.right - rect.left,
        h: rect.bottom - rect.top,
        output_idx: idx,
    };
    let seg_path = segment_path(&base, 0);
    match capture::start_segment(&app.cfg, params, &seg_path, None) {
        Ok((seg, entry, fmt)) => {
            app.recording = Some(Recording {
                params,
                base,
                segments: vec![entry],
                audio_fmt: fmt,
                seq: 0,
                seg: Some(seg),
                retried: false,
            });
            app.mode = Mode::Recording;
            update_menu(app.mode, items);
            overlay::show_border(rect);
            let _ = tray.set_tooltip(Some(format!("Snapper — recording ({name})")));
        }
        Err(e) => {
            eprintln!("capture: {e}");
            let _ = tray.set_tooltip(Some(format!("Snapper — error: {e}")));
        }
    }
}

fn poll_segment(app: &mut App, tray: &TrayIcon, items: &MenuItems) {
    let rec = match app.recording.as_mut() {
        Some(r) => r,
        None => return,
    };
    let seg = match rec.seg.as_mut() {
        Some(s) => s,
        None => return,
    };
    let done = match seg.try_done() {
        Ok(Some(code)) => Some(code),
        Ok(None) => return,
        Err(e) => {
            eprintln!("[app] segment wait error: {e}");
            Some(1)
        }
    };
    let code = match done {
        Some(c) => c,
        None => return,
    };
    rec.seg = None;
    if code != 0 && app.mode == Mode::Recording {
        // ffmpeg died on its own: retry the segment once with a software encoder
        if !rec.retried {
            rec.retried = true;
            if let Some(bad) = rec.segments.pop() {
                let _ = std::fs::remove_file(&bad.video);
                let _ = std::fs::remove_file(bad.video.with_extension("ffmpeg.log"));
                for a in &bad.audio {
                    let _ = std::fs::remove_file(a);
                }
            }
            eprintln!("[app] segment crashed, retrying with libx264");
            let seg_path = segment_path(&rec.base, rec.seq);
            match capture::start_segment(&app.cfg, rec.params, &seg_path, Some("libx264")) {
                Ok((seg, entry, fmt)) => {
                    rec.segments.push(entry);
                    if rec.audio_fmt.is_none() {
                        rec.audio_fmt = fmt;
                    }
                    rec.seg = Some(seg);
                    let _ = tray.set_tooltip(Some("Snapper — recording (recovered)"));
                    return;
                }
                Err(e) => {
                    eprintln!("[app] retry failed: {e}");
                }
            }
        }
        let _ = tray.set_tooltip(Some(&format!("Snapper — capture failed ({code})")));
        finalize(app, tray);
        return;
    }
    match app.mode {
        Mode::Stopping => finalize(app, tray),
        Mode::Pausing => {
            app.mode = Mode::Paused;
            update_menu(app.mode, items);
            let _ = tray.set_tooltip(Some("Snapper — paused"));
        }
        _ => {}
    }
}

fn finalize(app: &mut App, tray: &TrayIcon) {
    overlay::hide_border();
    let rec = match app.recording.take() {
        Some(r) => r,
        None => {
            app.mode = Mode::Idle;
            return;
        }
    };
    match capture::finalize(&app.cfg, &rec.base, &rec.segments, rec.audio_fmt) {
        Ok(()) => {
            app.last_out = Some(rec.base.clone());
            let label = rec
                .base
                .file_name()
                .map(|n| format!("Snapper — saved {}", n.to_string_lossy()))
                .unwrap_or_else(|| "Snapper — saved".into());
            let _ = tray.set_tooltip(Some(&label));
        }
        Err(e) => {
            eprintln!("finalize: {e}");
            let _ = tray.set_tooltip(Some(&format!("Snapper — finalize failed: {e}")));
        }
    }
    app.mode = Mode::Idle;
}

fn open_folder(app: &App) {
    let dir = app.cfg.output_dir.clone();
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}
