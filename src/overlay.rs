use crate::util::wide;
use anyhow::Result;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    COLOR_BTNFACE, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, DIB_RGB_COLORS,
    DEFAULT_GUI_FONT, GetStockObject, GetSysColorBrush, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_SELECTION: u32 = 0x8001;
pub const WM_APP_ACTION: u32 = 0x8002;
pub const WM_APP_CTL: u32 = 0x8003;

const DIM: u32 = 0x8000_0000; // black, alpha 128 (premultiplied)
const RED: u32 = 0xFF00_00FF; // opaque red (premultiplied)

fn get_x(lp: LPARAM) -> i32 {
    ((lp.0 as u32) as u16) as i16 as i32
}

fn get_y(lp: LPARAM) -> i32 {
    (((lp.0 as u32) >> 16) as u16) as i16 as i32
}

struct OverlayState {
    main: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dragging: bool,
    sx: i32,
    sy: i32,
    rect: Option<(i32, i32, i32, i32)>,
    bits: *mut u32,
    dib: HBITMAP,
    dc: HDC,
}

struct BorderState {
    hwnd: HWND,
    dib: HBITMAP,
    dc: HDC,
}

unsafe impl Send for OverlayState {}
unsafe impl Send for BorderState {}

static OVERLAY: Mutex<Option<OverlayState>> = Mutex::new(None);
static BORDER: Mutex<Option<BorderState>> = Mutex::new(None);
static RESULT: Mutex<Option<RECT>> = Mutex::new(None);
static ACTION_HWND: AtomicIsize = AtomicIsize::new(0);
static ACTION_MAIN: AtomicIsize = AtomicIsize::new(0);
static CTL_HWND: AtomicIsize = AtomicIsize::new(0);
static CTL_MAIN: AtomicIsize = AtomicIsize::new(0);
static CTL_PAUSE_BTN: AtomicIsize = AtomicIsize::new(0);

pub fn take_result() -> Option<RECT> {
    RESULT.lock().unwrap().take()
}

pub fn register_class() -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let cursor = LoadCursorW(HINSTANCE::default(), IDC_CROSS)?;

        let cls = wide("SnapperOverlay");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hinst.into(),
            hCursor: cursor,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow::anyhow!("RegisterClassExW overlay failed"));
        }

        let cls = wide("SnapperBorder");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(border_wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow::anyhow!("RegisterClassExW border failed"));
        }

        let cls = wide("SnapperAction");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(action_wndproc),
            hInstance: hinst.into(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow::anyhow!("RegisterClassExW action failed"));
        }

        let cls = wide("SnapperCtl");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(ctl_wndproc),
            hInstance: hinst.into(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow::anyhow!("RegisterClassExW ctl failed"));
        }
    }
    Ok(())
}

fn make_dib(dc: HDC, w: i32, h: i32) -> Result<(HBITMAP, *mut u32)> {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)
            .map_err(|_| anyhow::anyhow!("CreateDIBSection failed"))?;
        Ok((dib, bits as *mut u32))
    }
}

fn present(hwnd: HWND, dc: HDC, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pos = POINT { x, y };
        let src = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        if let Err(e) = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            Some(&pos),
            Some(&size),
            dc,
            Some(&src),
            COLORREF(0),
            Some(&bf),
            ULW_ALPHA,
        ) {
            eprintln!("[overlay] UpdateLayeredWindow failed: {e}");
        }
    }
}

pub fn show(main: HWND) -> Result<()> {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            return Err(anyhow::anyhow!("no virtual screen"));
        }

        let cls = wide("SnapperOverlay");
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            vx,
            vy,
            vw,
            vh,
            HWND::default(),
            HMENU::default(),
            GetModuleHandleW(None)?,
            None,
        )?;

        let dc = CreateCompatibleDC(None);
        let (dib, bits) = make_dib(dc, vw, vh)?;
        SelectObject(dc, HGDIOBJ(dib.0));

        let mut st = OverlayState {
            main,
            x: vx,
            y: vy,
            w: vw,
            h: vh,
            dragging: false,
            sx: 0,
            sy: 0,
            rect: None,
            bits,
            dib,
            dc,
        };
        paint_overlay(hwnd, &mut st);
        *OVERLAY.lock().unwrap() = Some(st);

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }
    Ok(())
}

unsafe fn paint_overlay(hwnd: HWND, st: &mut OverlayState) {
    let n = (st.w as usize) * (st.h as usize);
    let buf = std::slice::from_raw_parts_mut(st.bits, n);
    buf.fill(DIM);
    if let Some((x1, y1, x2, y2)) = st.rect {
        for y in y1..=y2 {
            let row = (y * st.w) as usize;
            for x in x1..=x2 {
                let border = x - x1 < 2 || x2 - x < 2 || y - y1 < 2 || y2 - y < 2;
                buf[row + x as usize] = if border { RED } else { 0 };
            }
        }
    }
    present(hwnd, st.dc, st.x, st.y, st.w, st.h);
}

unsafe extern "system" fn overlay_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let _ = SetCapture(hwnd);
            let mut g = OVERLAY.lock().unwrap();
            if let Some(st) = g.as_mut() {
                st.dragging = true;
                st.sx = get_x(lp).clamp(0, st.w - 1);
                st.sy = get_y(lp).clamp(0, st.h - 1);
                st.rect = None;
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut g = OVERLAY.lock().unwrap();
            if let Some(st) = g.as_mut() {
                if st.dragging {
                    let x = get_x(lp).clamp(0, st.w - 1);
                    let y = get_y(lp).clamp(0, st.h - 1);
                    let (x1, x2) = (st.sx.min(x), st.sx.max(x));
                    let (y1, y2) = (st.sy.min(y), st.sy.max(y));
                    st.rect = Some((x1, y1, x2, y2));
                    paint_overlay(hwnd, st);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let (main, ok) = {
                let mut g = OVERLAY.lock().unwrap();
                match g.as_mut() {
                    Some(st) => {
                        st.dragging = false;
                        let ok = st
                            .rect
                            .map_or(false, |r| (r.2 - r.0) >= 8 && (r.3 - r.1) >= 8);
                        if ok {
                            let (x1, y1, x2, y2) = st.rect.unwrap();
                            *RESULT.lock().unwrap() = Some(RECT {
                                left: x1 + st.x,
                                top: y1 + st.y,
                                right: x2 + 1 + st.x,
                                bottom: y2 + 1 + st.y,
                            });
                        }
                        (st.main, ok)
                    }
                    None => (HWND::default(), false),
                }
            };
            let _ = DestroyWindow(hwnd);
            let _ = PostMessageW(main, WM_APP_SELECTION, WPARAM(ok as usize), LPARAM(0));
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 as u32 == VK_ESCAPE.0 as u32 {
                let main = OVERLAY
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.main)
                    .unwrap_or_default();
                let _ = DestroyWindow(hwnd);
                let _ = PostMessageW(main, WM_APP_SELECTION, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            let main = OVERLAY
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.main)
                .unwrap_or_default();
            let _ = DestroyWindow(hwnd);
            let _ = PostMessageW(main, WM_APP_SELECTION, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut g = OVERLAY.lock().unwrap();
            if let Some(st) = g.take() {
                let _ = DeleteObject(HGDIOBJ(st.dib.0));
                let _ = DeleteDC(st.dc);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub fn show_border(rect: RECT) {
    unsafe {
        let pad = 4i32;
        let x = rect.left - pad;
        let y = rect.top - pad;
        let w = (rect.right - rect.left) + 2 * pad;
        let h = (rect.bottom - rect.top) + 2 * pad;

        let cls = wide("SnapperBorder");
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            w,
            h,
            HWND::default(),
            HMENU::default(),
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        );
        let Ok(hwnd) = hwnd else { return };

        let dc = CreateCompatibleDC(None);
        let Ok((dib, bits)) = make_dib(dc, w, h) else {
            let _ = DestroyWindow(hwnd);
            return;
        };
        SelectObject(dc, HGDIOBJ(dib.0));

        let n = (w as usize) * (h as usize);
        let buf = std::slice::from_raw_parts_mut(bits, n);
        buf.fill(0);
        for row in 0..h as usize {
            for col in 0..w as usize {
                if row < 3 || row as i32 >= h - 3 || col < 3 || col as i32 >= w - 3 {
                    buf[row * w as usize + col] = RED;
                }
            }
        }
        present(hwnd, dc, x, y, w, h);

        *BORDER.lock().unwrap() = Some(BorderState { hwnd, dib, dc });
    }
}

pub fn hide_border() {
    let hwnd = BORDER.lock().unwrap().as_ref().map(|b| b.hwnd);
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe extern "system" fn border_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            let mut g = BORDER.lock().unwrap();
            if let Some(st) = g.take() {
                let _ = DeleteObject(HGDIOBJ(st.dib.0));
                let _ = DeleteDC(st.dc);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Small topmost popup anchored under the selection: Record / Screenshot.
pub fn show_action_bar(main: HWND, rect: RECT) {
    unsafe {
        ACTION_MAIN.store(main.0 as isize, Ordering::SeqCst);
        let bar_w = 224i32;
        let bar_h = 44i32;
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let mut x = rect.right - bar_w;
        let mut y = rect.bottom + 6;
        if y + bar_h > vy + vh {
            y = (rect.top - bar_h - 6).max(vy);
        }
        x = x.clamp(vx, (vx + vw - bar_w).max(vx));

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cls = wide("SnapperAction");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            bar_w,
            bar_h,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        );
        let Ok(hwnd) = hwnd else { return };

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let btn_cls = wide("BUTTON");
        let mk = |label: &str, id: usize, bx: i32| {
            let w = wide(label);
            let b = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(btn_cls.as_ptr()),
                PCWSTR(w.as_ptr()),
                WS_CHILD | WS_VISIBLE,
                bx,
                8,
                100,
                28,
                hwnd,
                HMENU(id as *mut c_void),
                hinst,
                None,
            );
            if let Ok(b) = b {
                let _ = SendMessageW(b, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }
        };
        mk("Record", 1, 8);
        mk("Screenshot", 2, 116);

        ACTION_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn hide_action_bar() {
    let h = ACTION_HWND.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(h as *mut c_void));
        }
    }
}

unsafe extern "system" fn action_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, _lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as u16;
            let code = ((wp.0 >> 16) & 0xFFFF) as u16;
            if code == 0 {
                let main = HWND(ACTION_MAIN.load(Ordering::SeqCst) as *mut c_void);
                ACTION_HWND.store(0, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                let _ = PostMessageW(main, WM_APP_ACTION, WPARAM(id as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 as u32 == VK_ESCAPE.0 as u32 {
                let main = HWND(ACTION_MAIN.load(Ordering::SeqCst) as *mut c_void);
                ACTION_HWND.store(0, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                let _ = PostMessageW(main, WM_APP_ACTION, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, _lp),
    }
}

/// Floating control bar anchored just outside the recording region:
/// Pause/Resume + Stop. Posts WM_APP_CTL (1 = pause toggle, 2 = stop).
pub fn show_ctl_bar(main: HWND, rect: RECT) {
    unsafe {
        CTL_MAIN.store(main.0 as isize, Ordering::SeqCst);
        let bar_w = 168i32;
        let bar_h = 36i32;
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let mut x = rect.right - bar_w;
        let mut y = rect.top - bar_h - 8;
        if y < vy {
            y = (rect.bottom + 8).min(vy + vh - bar_h);
        }
        x = x.clamp(vx, (vx + vw - bar_w).max(vx));

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cls = wide("SnapperCtl");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            bar_w,
            bar_h,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        );
        let Ok(hwnd) = hwnd else { return };

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let btn_cls = wide("BUTTON");
        let mk = |label: &str, id: usize, bx: i32| {
            let w = wide(label);
            let b = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(btn_cls.as_ptr()),
                PCWSTR(w.as_ptr()),
                WS_CHILD | WS_VISIBLE,
                bx,
                6,
                76,
                24,
                hwnd,
                HMENU(id as *mut c_void),
                hinst,
                None,
            );
            if let Ok(b) = b {
                let _ = SendMessageW(b, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                if id == 1 {
                    CTL_PAUSE_BTN.store(b.0 as isize, Ordering::SeqCst);
                }
            }
        };
        mk("Pause", 1, 6);
        mk("Stop", 2, 86);

        CTL_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    }
}

pub fn update_ctl_pause(paused: bool) {
    let btn = CTL_PAUSE_BTN.load(Ordering::SeqCst);
    if btn == 0 {
        return;
    }
    let label = wide(if paused { "Resume" } else { "Pause" });
    unsafe {
        let _ = SetWindowTextW(HWND(btn as *mut c_void), PCWSTR(label.as_ptr()));
    }
}

pub fn hide_ctl_bar() {
    CTL_PAUSE_BTN.store(0, Ordering::SeqCst);
    let h = CTL_HWND.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(h as *mut c_void));
        }
    }
}

unsafe extern "system" fn ctl_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, _lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as u16;
            let code = ((wp.0 >> 16) & 0xFFFF) as u16;
            if code == 0 {
                let main = HWND(CTL_MAIN.load(Ordering::SeqCst) as *mut c_void);
                let _ = PostMessageW(main, WM_APP_CTL, WPARAM(id as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, _lp),
    }
}
