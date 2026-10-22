// Fullscreen selection overlay + recording border. Dark-minimal styling:
// 33% dim, 1px hairline selection edge (outside the region), live size badge,
// fade-in, thin accent recording border with soft outer glow.
use crate::theme;
use crate::ui;
use crate::util::wide;
use anyhow::Result;
use std::mem::size_of;
use std::sync::Mutex;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_SELECTION: u32 = 0x8001;
pub const WM_APP_ACTION: u32 = 0x8002;
pub const WM_APP_CTL: u32 = 0x8003;

const FADE_TIMER: usize = 0xF1;

struct OverlayState {
    main: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dragging: bool,
    sx: i32,
    sy: i32,
    cx: i32,
    cy: i32,
    rect: Option<(i32, i32, i32, i32)>,
    bits: *mut u32,
    dib: HBITMAP,
    dc: HDC,
    rt: ID2D1DCRenderTarget,
    alpha: f32,
    sc: f32,
}

struct BorderState {
    hwnd: HWND,
    dib: HBITMAP,
    dc: HDC,
    bits: *mut u32,
    rt: ID2D1DCRenderTarget,
    w: i32,
    h: i32,
    sc: f32,
}

unsafe impl Send for OverlayState {}
unsafe impl Send for BorderState {}

static OVERLAY: Mutex<Option<OverlayState>> = Mutex::new(None);
static BORDER: Mutex<Option<BorderState>> = Mutex::new(None);
static RESULT: Mutex<Option<RECT>> = Mutex::new(None);

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
    }
    Ok(())
}

unsafe fn present(hwnd: HWND, dc: HDC, x: i32, y: i32, w: i32, h: i32) {
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

        let sc = theme::scale_for_rect(RECT {
            left: vx,
            top: vy,
            right: vx + vw,
            bottom: vy + vh,
        });
        // rt dpi stays 96: all draw coords are physical px; fonts/strokes scaled by sc
        let (dib, dc, bits, rt) = ui::make_surface(vw, vh, 1.0)?;

        let st = OverlayState {
            main,
            x: vx,
            y: vy,
            w: vw,
            h: vh,
            dragging: false,
            sx: 0,
            sy: 0,
            cx: 0,
            cy: 0,
            rect: None,
            bits,
            dib,
            dc,
            rt,
            alpha: 0.0,
            sc,
        };
        paint_overlay(&st);
        present(hwnd, dc, vx, vy, vw, vh);
        *OVERLAY.lock().unwrap() = Some(st);

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = SetTimer(hwnd, FADE_TIMER, 16, None);
    }
    Ok(())
}

unsafe fn paint_overlay(st: &OverlayState) {
    let n = (st.w as usize) * (st.h as usize);
    std::slice::from_raw_parts_mut(st.bits, n).fill(0);
    let _ = st.rt.BindDC(st.dc, &RECT { left: 0, top: 0, right: st.w, bottom: st.h });
    let Ok(rt) = st.rt.cast::<ID2D1RenderTarget>() else {
        return;
    };
    let _ = rt.BeginDraw();
    let a = st.alpha;
    let w = st.w as f32;
    let h = st.h as f32;
    let dim = rt.CreateSolidColorBrush(&theme::dim(), None).unwrap();
    dim.SetOpacity(a);
    match st.rect {
        Some((x1, y1, x2, y2)) => {
            let x1 = x1 as f32;
            let y1 = y1 as f32;
            let x2 = x2 as f32;
            let y2 = y2 as f32;
            rt.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: y1 + 1.0 }, &dim);
            rt.FillRectangle(&D2D_RECT_F { left: 0.0, top: y2 + 1.0, right: w, bottom: h }, &dim);
            rt.FillRectangle(&D2D_RECT_F { left: 0.0, top: y1 + 1.0, right: x1, bottom: y2 + 1.0 }, &dim);
            rt.FillRectangle(&D2D_RECT_F { left: x2 + 1.0, top: y1 + 1.0, right: w, bottom: y2 + 1.0 }, &dim);

            // hairline selection edge, drawn outside the captured region
            let hl = rt.CreateSolidColorBrush(&theme::col(255, 255, 255, 0.85), None).unwrap();
            hl.SetOpacity(a);
            rt.DrawRectangle(
                &D2D_RECT_F {
                    left: x1 - 1.0,
                    top: y1 - 1.0,
                    right: x2 + 2.0,
                    bottom: y2 + 2.0,
                },
                &hl,
                1.0,
                None,
            );

            if st.dragging {
                paint_badge(st, x2 - x1 + 1.0, y2 - y1 + 1.0);
            }
        }
        None => {
            rt.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: h }, &dim);
        }
    }
    let _ = st.rt.EndDraw(None, None);
}

/// live "W × H" capsule following the cursor
unsafe fn paint_badge(st: &OverlayState, rw: f32, rh: f32) {
    let Ok(rt) = st.rt.cast::<ID2D1RenderTarget>() else {
        return;
    };
    let sc = st.sc;
    let txt = format!("{} \u{00d7} {}", rw as i32, rh as i32);
    let bw = txt.len() as f32 * 7.5 * sc + 24.0 * sc;
    let bh = 24.0 * sc;
    let mut bx = st.cx as f32 + 14.0 * sc;
    let mut by = st.cy as f32 + 14.0 * sc;
    if bx + bw > st.w as f32 {
        bx = st.cx as f32 - bw - 14.0 * sc;
    }
    if by + bh > st.h as f32 {
        by = st.cy as f32 - bh - 14.0 * sc;
    }
    let capsule = D2D_RECT_F {
        left: bx,
        top: by,
        right: bx + bw,
        bottom: by + bh,
    };
    let br = rt.CreateSolidColorBrush(&theme::surface(), None).unwrap();
    br.SetOpacity(st.alpha);
    rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: capsule,
            radiusX: bh / 2.0,
            radiusY: bh / 2.0,
        },
        &br,
    );
    let hl = rt.CreateSolidColorBrush(&theme::hairline(), None).unwrap();
    hl.SetOpacity(st.alpha);
    rt.DrawRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: capsule,
            radiusX: bh / 2.0,
            radiusY: bh / 2.0,
        },
        &hl,
        1.0,
        None,
    );
    let f = ui::ui_format_pub(13.0 * sc);
    let tb = rt.CreateSolidColorBrush(&theme::text(), None).unwrap();
    tb.SetOpacity(st.alpha);
    let tw = wide(&txt);
    rt.DrawText(
        &tw[..tw.len() - 1],
        &f,
        &capsule,
        &tb,
        D2D1_DRAW_TEXT_OPTIONS_NONE,
        DWRITE_MEASURING_MODE_NATURAL,
    );
}

fn get_x(lp: LPARAM) -> i32 {
    ((lp.0 as u32) as u16) as i16 as i32
}

fn get_y(lp: LPARAM) -> i32 {
    (((lp.0 as u32) >> 16) as u16) as i16 as i32
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
                st.cx = st.sx;
                st.cy = st.sy;
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
                    st.cx = x;
                    st.cy = y;
                    let (x1, x2) = (st.sx.min(x), st.sx.max(x));
                    let (y1, y2) = (st.sy.min(y), st.sy.max(y));
                    st.rect = Some((x1, y1, x2, y2));
                    paint_overlay(st);
                    present(hwnd, st.dc, st.x, st.y, st.w, st.h);
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
        WM_TIMER if wp.0 == FADE_TIMER => {
            let mut g = OVERLAY.lock().unwrap();
            if let Some(st) = g.as_mut() {
                st.alpha = (st.alpha + 0.16).min(1.0);
                paint_overlay(st);
                present(hwnd, st.dc, st.x, st.y, st.w, st.h);
                if st.alpha >= 1.0 {
                    let _ = KillTimer(hwnd, FADE_TIMER);
                }
            }
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

// ---------------------------------------------------------------- border

pub fn show_border(rect: RECT) {
    unsafe {
        let sc = theme::scale_for_rect(rect);
        let pad = (4.0 * sc) as i32;
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

        let Ok((dib, dc, bits, rt)) = ui::make_surface(w, h, 1.0) else {
            let _ = DestroyWindow(hwnd);
            return;
        };

        let st = BorderState {
            hwnd,
            dib,
            dc,
            bits,
            rt,
            w,
            h,
            sc,
        };
        paint_border(&st);
        present(hwnd, dc, x, y, w, h);
        *BORDER.lock().unwrap() = Some(st);
    }
}

unsafe fn paint_border(st: &BorderState) {
    let n = (st.w as usize) * (st.h as usize);
    std::slice::from_raw_parts_mut(st.bits, n).fill(0);
    let _ = st.rt.BindDC(st.dc, &RECT { left: 0, top: 0, right: st.w, bottom: st.h });
    let Ok(rt) = st.rt.cast::<ID2D1RenderTarget>() else {
        return;
    };
    let _ = rt.BeginDraw();
    let sc = st.sc;
    let edge = 2.0 * sc;
    let r = D2D_RECT_F {
        left: edge,
        top: edge,
        right: st.w as f32 - edge,
        bottom: st.h as f32 - edge,
    };
    // soft outer glow, then thin accent line
    let glow = rt.CreateSolidColorBrush(&theme::accent(), None).unwrap();
    glow.SetOpacity(0.22);
    rt.DrawRectangle(&r, &glow, 6.0 * sc, None);
    glow.SetOpacity(0.45);
    rt.DrawRectangle(&r, &glow, 3.0 * sc, None);
    let line = rt.CreateSolidColorBrush(&theme::accent(), None).unwrap();
    rt.DrawRectangle(&r, &line, 1.5 * sc, None);
    let _ = st.rt.EndDraw(None, None);
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
