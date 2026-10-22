// Release: GUI subsystem so launching from Explorer/Shell shows no console
// window — the app is tray-only. Debug keeps the console for e2e stderr.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod config;
mod overlay;
mod theme;
mod ui;
mod util;

use anyhow::{Context, Result};
use std::mem::size_of;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
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

pub use overlay::{WM_APP_ACTION, WM_APP_CTL, WM_APP_SELECTION};
pub use ui::WM_APP_MENU;
const HOTKEY_REC: i32 = 1;
const HOTKEY_PAUSE: i32 = 2;

#[derive(PartialEq, Clone, Copy, Debug)]
enum Mode {
    Idle,
    Selecting,
    Choosing,
    Recording,
    Paused,
    Stopping,
}

struct PendingSel {
    params: capture::SegParams,
    rect: windows::Win32::Foundation::RECT,
    base: PathBuf,
    name: String,
}

struct App {
    cfg: config::Config,
    mode: Mode,
    recording: Option<Recording>,
    pending: Option<PendingSel>,
    last_out: Option<PathBuf>,
    pause_label: String,
}

struct Recording {
    base: PathBuf,
    raw: PathBuf,
    seg: Option<capture::Segment>,
    paused_wall: Vec<(f64, f64)>,
    pause_start_wall: Option<f64>,
    total_wall: f64,
}

unsafe extern "system" fn main_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn tray_state(mode: Mode) -> u8 {
    match mode {
        Mode::Recording => 1,
        Mode::Paused => 2,
        _ => 0,
    }
}

fn make_tray_icon(mode: Mode) -> Icon {
    unsafe {
        let sc = theme::scale_primary();
        let size = ((GetSystemMetrics(SM_CXSMICON)) as f32 * sc)
            .round()
            .max(16.0) as i32;
        let rgba = ui::render_tray_icon(tray_state(mode), size);
        Icon::from_rgba(rgba, size as u32, size as u32).expect("icon")
    }
}

fn sync_tray(mode: Mode, tray: &TrayIcon) {
    let _ = tray.set_icon(Some(make_tray_icon(mode)));
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
        pending: None,
        last_out: None,
        pause_label: String::new(),
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
    app.pause_label = pause_label.unwrap_or("Win+Shift+P").to_string();
    eprintln!("[app] pause hotkey: {}", app.pause_label);
    overlay::register_class()?;
    ui::register_class()?;

    let tray = TrayIconBuilder::new()
        .with_tooltip("Snapper — Win+Shift+R to record")
        .with_icon(make_tray_icon(Mode::Idle))
        .build()
        .map_err(|e| anyhow::anyhow!("tray: {e:?}"))?;

    let mut msg = MSG::default();
    let mut last_tick = Instant::now();
    loop {
        let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
        if got.as_bool() {
            match msg.message {
                WM_QUIT => break,
                WM_HOTKEY => match msg.wParam.0 as i32 {
                    HOTKEY_REC => {
                        ui::hide_menu();
                        toggle_rec(&mut app, main, &tray)
                    }
                    HOTKEY_PAUSE => toggle_pause(&mut app, &tray),
                    _ => {}
                },
                WM_APP_SELECTION => on_selection(&mut app, msg.wParam.0, main, &tray),
                WM_APP_ACTION => on_action(&mut app, msg.wParam.0, main, &tray),
                WM_APP_CTL => on_ctl(&mut app, msg.wParam.0, &tray),
                WM_APP_MENU => match msg.wParam.0 {
                    1 => toggle_rec(&mut app, main, &tray),
                    2 | 3 => toggle_pause(&mut app, &tray),
                    4 => stop_rec(&mut app, &tray),
                    5 => open_folder(&app),
                    6 => break,
                    _ => {}
                },
                _ => {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        } else {
            poll_segment(&mut app, &tray);
            if let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    button_state: MouseButtonState::Down,
                    ..
                } = ev
                {
                    ui::show_menu(
                        main,
                        app.mode == Mode::Idle,
                        app.mode == Mode::Recording,
                        app.mode == Mode::Paused,
                        &app.pause_label.clone(),
                    );
                }
            }
            tick_timer(&mut app, &mut last_tick);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    ui::hide_menu();

    // quitting while recording: stop ffmpeg and salvage the file
    if let Some(rec) = app.recording.as_mut() {
        let mut total_wall = rec.total_wall;
        if let Some(seg) = rec.seg.as_mut() {
            if total_wall == 0.0 {
                total_wall = seg.elapsed();
            }
            seg.stop_signal();
            std::thread::sleep(Duration::from_millis(500));
        }
        rec.seg = None;
        let mut paused = rec.paused_wall.clone();
        if let Some(ps) = rec.pause_start_wall {
            paused.push((ps, total_wall));
        }
        let _ = capture::finalize(&app.cfg, &rec.base, &rec.raw, &paused, total_wall);
    }
    Ok(())
}

fn tick_timer(app: &mut App, last_tick: &mut Instant) {
    if !matches!(app.mode, Mode::Recording | Mode::Paused) {
        return;
    }
    if last_tick.elapsed() < Duration::from_millis(500) {
        return;
    }
    *last_tick = Instant::now();
    let Some(rec) = app.recording.as_mut() else {
        return;
    };
    let Some(seg) = rec.seg.as_mut() else {
        return;
    };
    // freeze the readout at the pause point while paused
    let e = rec.pause_start_wall.unwrap_or_else(|| seg.elapsed());
    let t = format!("{}:{:02}", (e / 60.0) as u32, (e as u32) % 60);
    ui::ctl_set_state(app.mode == Mode::Paused, Some(&t));
}

fn toggle_rec(app: &mut App, main: HWND, tray: &TrayIcon) {
    match app.mode {
        Mode::Idle => {
            app.mode = Mode::Selecting;
            sync_tray(app.mode, tray);
            if let Err(e) = overlay::show(main) {
                eprintln!("overlay: {e}");
                app.mode = Mode::Idle;
                sync_tray(app.mode, tray);
            }
        }
        Mode::Recording => stop_rec(app, tray),
        Mode::Choosing => {
            ui::hide_action_bar();
            app.pending = None;
            app.mode = Mode::Idle;
            sync_tray(app.mode, tray);
        }
        Mode::Paused => stop_rec(app, tray),
        _ => {}
    }
}

fn toggle_pause(app: &mut App, tray: &TrayIcon) {
    match app.mode {
        Mode::Recording => {
            if let Some(rec) = app.recording.as_mut() {
                if let Some(seg) = rec.seg.as_mut() {
                    // ffmpeg keeps running; we just mark where the pause begins
                    // on the wall clock and cut it out at stop.
                    rec.pause_start_wall = Some(seg.elapsed());
                    app.mode = Mode::Paused;
                    sync_tray(app.mode, tray);
                    ui::ctl_set_state(true, None);
                    let _ = tray.set_tooltip(Some("Snapper — paused"));
                }
            }
        }
        Mode::Paused => {
            let rec = match app.recording.as_mut() {
                Some(r) => r,
                None => return,
            };
            if let (Some(seg), Some(ps)) = (rec.seg.as_mut(), rec.pause_start_wall.take()) {
                rec.paused_wall.push((ps, seg.elapsed()));
            }
            app.mode = Mode::Recording;
            sync_tray(app.mode, tray);
            ui::ctl_set_state(false, None);
            let _ = tray.set_tooltip(Some("Snapper — recording"));
        }
        _ => {}
    }
}

fn stop_rec(app: &mut App, tray: &TrayIcon) {
    match app.mode {
        Mode::Recording | Mode::Paused => {}
        _ => return,
    }
    app.mode = Mode::Stopping;
    sync_tray(app.mode, tray);
    if let Some(rec) = app.recording.as_mut() {
        if let Some(seg) = rec.seg.as_mut() {
            rec.total_wall = seg.elapsed();
            seg.stop_signal();
            let _ = tray.set_tooltip(Some("Snapper — stopping…"));
        } else {
            finalize(app, tray);
        }
    }
}

fn raw_path(base: &PathBuf) -> PathBuf {
    base.with_extension("raw.mp4")
}

fn on_selection(app: &mut App, ok: usize, main: HWND, tray: &TrayIcon) {
    app.mode = Mode::Idle;
    sync_tray(app.mode, tray);
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
    app.pending = Some(PendingSel {
        params,
        rect,
        base,
        name,
    });
    app.mode = Mode::Choosing;
    sync_tray(app.mode, tray);
    let _ = tray.set_tooltip(Some("Snapper — Record or Screenshot? (Esc cancels)"));
    ui::show_action_bar(main, rect);
}

fn on_action(app: &mut App, choice: usize, main: HWND, tray: &TrayIcon) {
    ui::hide_action_bar();
    let Some(pend) = app.pending.take() else {
        app.mode = Mode::Idle;
        sync_tray(app.mode, tray);
        return;
    };
    app.mode = Mode::Idle;
    sync_tray(app.mode, tray);
    match choice {
        1 => start_recording(app, pend, main, tray),
        2 => take_screenshot(app, &pend, tray),
        _ => {
            let _ = tray.set_tooltip(Some("Snapper — cancelled"));
        }
    }
}

fn on_ctl(app: &mut App, id: usize, tray: &TrayIcon) {
    match id {
        1 => toggle_pause(app, tray),
        2 => stop_rec(app, tray),
        _ => {}
    }
}

fn start_recording(app: &mut App, pend: PendingSel, main: HWND, tray: &TrayIcon) {
    let raw = raw_path(&pend.base);
    // instant feedback: border + control bar BEFORE ffmpeg spawns, so the
    // user never sees a "nothing happened" gap that then lands in the video
    overlay::show_border(pend.rect);
    ui::show_ctl_bar(main, pend.rect);
    match capture::start_segment(&app.cfg, pend.params, &raw, None) {
        Ok(seg) => {
            app.recording = Some(Recording {
                base: pend.base,
                raw,
                seg: Some(seg),
                paused_wall: Vec::new(),
                pause_start_wall: None,
                total_wall: 0.0,
            });
            app.mode = Mode::Recording;
            sync_tray(app.mode, tray);
            let _ = tray.set_tooltip(Some(format!("Snapper — recording ({})", pend.name)));
        }
        Err(e) => {
            overlay::hide_border();
            ui::hide_ctl_bar();
            eprintln!("capture: {e}");
            let _ = tray.set_tooltip(Some(format!("Snapper — error: {e}")));
        }
    }
}

fn take_screenshot(app: &mut App, pend: &PendingSel, tray: &TrayIcon) {
    let out = app.cfg.screenshot_dir().join(format!("{}.png", pend.name));
    match capture::grab_frame(&app.cfg, pend.params, &out) {
        Ok(()) => {
            app.last_out = Some(out.clone());
            let label = out
                .file_name()
                .map(|n| format!("Snapper — saved {}", n.to_string_lossy()))
                .unwrap_or_else(|| "Snapper — saved".into());
            let _ = tray.set_tooltip(Some(&label));
        }
        Err(e) => {
            eprintln!("screenshot: {e}");
            let _ = tray.set_tooltip(Some(&format!("Snapper — screenshot failed: {e}")));
        }
    }
}

fn poll_segment(app: &mut App, tray: &TrayIcon) {
    let rec = match app.recording.as_mut() {
        Some(r) => r,
        None => return,
    };
    let seg = match rec.seg.as_mut() {
        Some(s) => s,
        None => return,
    };
    match seg.try_done() {
        Ok(None) => return,
        Ok(Some(code)) => {
            if rec.total_wall == 0.0 {
                rec.total_wall = seg.elapsed();
            }
            rec.seg = None;
            if code != 0 && app.mode == Mode::Recording {
                let _ = tray.set_tooltip(Some(&format!("Snapper — capture stopped ({code})")));
            }
            finalize(app, tray);
        }
        Err(e) => {
            eprintln!("[app] capture wait error: {e}");
            rec.seg = None;
            finalize(app, tray);
        }
    }
}

fn finalize(app: &mut App, tray: &TrayIcon) {
    overlay::hide_border();
    ui::hide_ctl_bar();
    let rec = match app.recording.take() {
        Some(r) => r,
        None => {
            app.mode = Mode::Idle;
            return;
        }
    };
    let mut paused = rec.paused_wall.clone();
    // stopping while paused: the open pause runs to the end → cut it too
    if let Some(ps) = rec.pause_start_wall {
        paused.push((ps, rec.total_wall));
    }
    match capture::finalize(&app.cfg, &rec.base, &rec.raw, &paused, rec.total_wall) {
        Ok(()) => {
            app.last_out = Some(rec.base.clone());
            let fname = rec
                .base
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let _ = tray.set_tooltip(Some(&format!("Snapper — saved {fname}")));
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
