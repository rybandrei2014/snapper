mod audio;
mod capture;
mod config;
mod overlay;
mod util;

use anyhow::{Context, Result};
use std::mem::size_of;
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
    RegisterHotKey, MOD_SHIFT, MOD_WIN, VK_R,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub use overlay::WM_APP_SELECTION;
pub const WM_APP_REC_STOPPED: u32 = 0x8002;
const HOTKEY_ID: i32 = 1;

#[derive(PartialEq, Clone, Copy, Debug)]
enum Mode {
    Idle,
    Selecting,
    Recording,
}

struct App {
    cfg: config::Config,
    mode: Mode,
    recorder: Option<capture::Recorder>,
    last_out: Option<std::path::PathBuf>,
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
        recorder: None,
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
    unsafe { RegisterHotKey(main, HOTKEY_ID, MOD_WIN | MOD_SHIFT, VK_R.0 as u32) }
        .context("hotkey Win+Shift+R is taken")?;
    overlay::register_class()?;

    let menu = Menu::new();
    let item_start = MenuItem::new("Start recording  (Win+Shift+R)", true, None);
    let item_stop = MenuItem::new("Stop recording", false, None);
    let item_open = MenuItem::new("Open output folder", true, None);
    let sep = PredefinedMenuItem::separator();
    let item_quit = MenuItem::new("Quit", true, None);
    let _ = menu.append(&item_start);
    let _ = menu.append(&item_stop);
    let _ = menu.append(&item_open);
    let _ = menu.append(&sep);
    let _ = menu.append(&item_quit);
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
                WM_HOTKEY => toggle(&mut app, main, &tray),
                WM_APP_SELECTION => on_selection(&mut app, msg.wParam.0, main, &tray),
                WM_APP_REC_STOPPED => on_stopped(&mut app, msg.wParam.0, &tray),
                _ => {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        } else {
            if let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id() == item_start.id() {
                    toggle(&mut app, main, &tray);
                } else if ev.id() == item_stop.id() {
                    stop_rec(&mut app);
                } else if ev.id() == item_open.id() {
                    open_folder(&app);
                } else if ev.id() == item_quit.id() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    if let Some(r) = app.recorder.as_mut() {
        r.stop();
    }
    Ok(())
}

fn toggle(app: &mut App, main: HWND, _tray: &TrayIcon) {
    match app.mode {
        Mode::Idle => {
            app.mode = Mode::Selecting;
            if let Err(e) = overlay::show(main) {
                eprintln!("overlay: {e}");
                app.mode = Mode::Idle;
            }
        }
        Mode::Selecting => {}
        Mode::Recording => stop_rec(app),
    }
}

fn stop_rec(app: &mut App) {
    if let Some(r) = app.recorder.as_mut() {
        r.stop();
    }
}

fn on_selection(app: &mut App, ok: usize, main: HWND, tray: &TrayIcon) {
    app.mode = Mode::Idle;
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
    let (mx, my) = (rect.left - mon.left, rect.top - mon.top);
    let (mw, mh) = (rect.right - rect.left, rect.bottom - rect.top);

    let name = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let out = app.cfg.output_dir.join(format!("{name}.mp4"));
    match capture::start(&app.cfg, mx, my, mw, mh, idx, main, out.clone()) {
        Ok(rec) => {
            app.recorder = Some(rec);
            app.mode = Mode::Recording;
            app.last_out = Some(out);
            overlay::show_border(rect);
            let _ = tray.set_tooltip(Some(format!("Snapper — recording ({name})")));
        }
        Err(e) => {
            eprintln!("capture: {e}");
            let _ = tray.set_tooltip(Some(format!("Snapper — error: {e}")));
        }
    }
}

fn on_stopped(app: &mut App, code: usize, tray: &TrayIcon) {
    app.recorder = None;
    app.mode = Mode::Idle;
    overlay::hide_border();
    let label = if code == 0 {
        app.last_out
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| format!("Snapper — saved {}", n.to_string_lossy()))
            .unwrap_or_else(|| "Snapper — saved".into())
    } else {
        format!("Snapper — recording failed (ffmpeg exit {code})")
    };
    let _ = tray.set_tooltip(Some(&label));
}

fn open_folder(app: &App) {
    let dir = app.cfg.output_dir.clone();
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}
