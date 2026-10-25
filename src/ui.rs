// Direct2D/DirectWrite capsule renderer: floating action bar, recording control
// bar, and the tray context menu. All surfaces are layered windows with
// per-pixel alpha drawn by D2D (premultiplied BGRA) and presented with
// UpdateLayeredWindow.
use crate::overlay::{WM_APP_ACTION, WM_APP_CTL};
use crate::theme;
use crate::util::wide;
use anyhow::Result;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Mutex, OnceLock};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_MENU: u32 = 0x8004;
// test hook: SendMessageW(bar, WM_SNAP_CLICK, button_id, 0)
pub const WM_SNAP_CLICK: u32 = 0x8010;
// windows-rs 0.58 puts this in Win32_UI_Controls; define it here instead
const WM_MOUSELEAVE: u32 = 0x02A3;

const KIND_ACTION: u8 = 0;
const KIND_CTL: u8 = 1;
const KIND_MENU: u8 = 2;

const FADE_TIMER: usize = 0xF0;

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Record,
    Camera,
    Close,
    Pause,
    Play,
    Stop,
    Folder,
    Power,
}

#[derive(Clone)]
struct Btn {
    id: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    icon: Icon,
    enabled: bool,
    label: String,
    shortcut: String,
}

struct Surface {
    hwnd: HWND,
    kind: u8,
    main: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    sc: f32,
    btns: Vec<Btn>,
    sep_after: usize,
    hover: isize,
    timer: String,
    paused: bool,
    alpha: f32,
    dc: HDC,
    dib: HBITMAP,
    bits: *mut u32,
    rt: ID2D1DCRenderTarget,
}

unsafe impl Send for Surface {}

static CAPSULE: Mutex<Option<Surface>> = Mutex::new(None);
static MENU: Mutex<Option<Surface>> = Mutex::new(None);

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub fn d2d_factory() -> &'static ID2D1Factory {
    static F: OnceLock<ID2D1Factory> = OnceLock::new();
    F.get_or_init(|| unsafe {
        D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).expect("d2d factory")
    })
}

pub fn dw_factory() -> &'static IDWriteFactory {
    static F: OnceLock<IDWriteFactory> = OnceLock::new();
    F.get_or_init(|| unsafe {
        DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("dwrite factory")
    })
}

unsafe fn make_text_format(size_px: f32, family: &str) -> IDWriteTextFormat {
    let fam = u16s(family);
    dw_factory()
        .CreateTextFormat(
            PCWSTR(fam.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size_px,
            w!(""),
        )
        .expect("text format")
}

unsafe fn glyph_format(px: f32) -> IDWriteTextFormat {
    let f = make_text_format(px, "Segoe MDL2 Assets");
    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    f
}

unsafe fn ui_format(px: f32) -> IDWriteTextFormat {
    let f = make_text_format(px, "Segoe UI");
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    f
}

pub unsafe fn ui_format_pub(px: f32) -> IDWriteTextFormat {
    ui_format(px)
}

/// premultiplied-BGRA DIB + DC render target sized to match, ready to draw
pub(crate) unsafe fn make_surface(
    w: i32,
    h: i32,
    sc: f32,
) -> Result<(HBITMAP, HDC, *mut u32, ID2D1DCRenderTarget)> {
    let dc = CreateCompatibleDC(None);
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
    let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)?;
    SelectObject(dc, HGDIOBJ(dib.0));
    let rt = d2d_factory().CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0 * sc,
        dpiY: 96.0 * sc,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    })?;
    rt.BindDC(dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
    Ok((dib, dc, bits as *mut u32, rt))
}

pub fn register_class() -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        type WP = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
        for (name, proc) in [
            ("SnapperAction", capsule_proc as WP),
            ("SnapperCtl", capsule_proc),
            ("SnapperMenu", menu_proc),
        ] {
            let cls = wide(name);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc),
                hInstance: hinst.into(),
                lpszClassName: PCWSTR(cls.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return Err(anyhow::anyhow!("RegisterClassExW {name} failed"));
            }
        }
    }
    Ok(())
}

unsafe fn create_layered(class: &str, x: i32, y: i32, w: i32, h: i32) -> Result<HWND> {
    let cls = wide(class);
    Ok(CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR(cls.as_ptr()),
        PCWSTR::null(),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
        HWND::default(),
        HMENU::default(),
        GetModuleHandleW(None)?,
        None,
    )?)
}

unsafe fn new_surface(
    class: &str,
    kind: u8,
    main: HWND,
    cx: i32,
    cy: i32,
    cw: i32,
    ch: i32,
    sc: f32,
    btns: Vec<Btn>,
    sep_after: usize,
) -> Result<Surface> {
    let m = (theme::SHADOW_PAD * sc).round() as i32;
    let (w, h) = (cw + 2 * m, ch + 2 * m);
    let hwnd = create_layered(class, cx - m, cy - m, w, h)?;
    let (dib, dc, bits, rt) = make_surface(w, h, sc)?;
    Ok(Surface {
        hwnd,
        kind,
        main,
        x: cx - m,
        y: cy - m,
        w,
        h,
        sc,
        btns,
        sep_after,
        hover: -1,
        timer: String::new(),
        paused: false,
        alpha: 0.0,
        dc,
        dib,
        bits,
        rt,
    })
}

fn anchor(rect: RECT, cw: i32, ch: i32, below: bool) -> (i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let mut x = rect.right - cw;
        let mut y = if below { rect.bottom + 6 } else { rect.top - ch - 8 };
        if below {
            if y + ch > vy + vh {
                y = (rect.top - ch - 6).max(vy);
            }
        } else if y < vy {
            y = (rect.bottom + 8).min(vy + vh - ch);
        }
        x = x.clamp(vx, (vx + vw - cw).max(vx));
        (x, y)
    }
}

// ---------------------------------------------------------------- bars

pub fn show_action_bar(main: HWND, rect: RECT) {
    unsafe {
        let sc = theme::scale_for_rect(rect);
        let w_dip = theme::PAD * 2.0 + theme::BTN * 3.0 + theme::GAP * 2.0;
        let cw = (w_dip * sc).round() as i32;
        let ch = (theme::BAR_H * sc).round() as i32;
        let (x, y) = anchor(rect, cw, ch, true);
        let m = theme::SHADOW_PAD * sc;
        let half = theme::BTN * sc / 2.0;
        let cy = ch as f32 / 2.0 + m;
        let bx = |i: f32| theme::PAD * sc + i * (theme::BTN + theme::GAP) * sc + half + m;
        let btns = vec![
            Btn { id: 1, x: bx(0.0) - half, y: cy - half, w: theme::BTN * sc, h: theme::BTN * sc, icon: Icon::Record, enabled: true, label: String::new(), shortcut: String::new() },
            Btn { id: 2, x: bx(1.0) - half, y: cy - half, w: theme::BTN * sc, h: theme::BTN * sc, icon: Icon::Camera, enabled: true, label: String::new(), shortcut: String::new() },
            Btn { id: 3, x: bx(2.0) - half, y: cy - half, w: theme::BTN * sc, h: theme::BTN * sc, icon: Icon::Close, enabled: true, label: String::new(), shortcut: String::new() },
        ];
        let s = new_surface("SnapperAction", KIND_ACTION, main, x, y, cw, ch, sc, btns, usize::MAX);
        let Ok(s) = s else { return };
        repaint(&s);
        present(&s);
        *CAPSULE.lock().unwrap() = Some(s);
        let hwnd = CAPSULE.lock().unwrap().as_ref().unwrap().hwnd;
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(hwnd, FADE_TIMER, 16, None);
    }
}

pub fn show_ctl_bar(main: HWND, rect: RECT) {
    unsafe {
        let sc = theme::scale_for_rect(rect);
        let w_dip = theme::PAD * 2.0 + theme::BTN * 2.0 + theme::GAP + theme::TIMER_W;
        let cw = (w_dip * sc).round() as i32;
        let ch = (theme::BAR_H * sc).round() as i32;
        let (x, y) = anchor(rect, cw, ch, false);
        let m = theme::SHADOW_PAD * sc;
        let half = theme::BTN * sc / 2.0;
        let cy = ch as f32 / 2.0 + m;
        let bx = |i: f32| theme::PAD * sc + i * (theme::BTN + theme::GAP) * sc + half + m;
        let btns = vec![
            Btn { id: 1, x: bx(0.0) - half, y: cy - half, w: theme::BTN * sc, h: theme::BTN * sc, icon: Icon::Pause, enabled: true, label: String::new(), shortcut: String::new() },
            Btn { id: 2, x: bx(1.0) - half, y: cy - half, w: theme::BTN * sc, h: theme::BTN * sc, icon: Icon::Stop, enabled: true, label: String::new(), shortcut: String::new() },
        ];
        let s = new_surface("SnapperCtl", KIND_CTL, main, x, y, cw, ch, sc, btns, usize::MAX);
        let Ok(mut s) = s else { return };
        s.timer = "0:00".into();
        repaint(&s);
        present(&s);
        *CAPSULE.lock().unwrap() = Some(s);
        let hwnd = CAPSULE.lock().unwrap().as_ref().unwrap().hwnd;
        let _ = SetTimer(hwnd, FADE_TIMER, 16, None);
    }
}

pub fn hide_action_bar() {
    hide_from(&CAPSULE);
}
pub fn hide_ctl_bar() {
    hide_from(&CAPSULE);
}
fn hide_from(slot: &'static Mutex<Option<Surface>>) {
    let h = slot.lock().unwrap().as_ref().map(|s| s.hwnd);
    if let Some(h) = h {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

pub fn ctl_set_state(paused: bool, timer: Option<&str>) {
    let mut g = CAPSULE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.kind != KIND_CTL {
        return;
    }
    s.paused = paused;
    s.btns[0].icon = if paused { Icon::Play } else { Icon::Pause };
    if let Some(t) = timer {
        s.timer = t.to_string();
    }
    unsafe {
        repaint(s);
        present(s);
    }
}

// ---------------------------------------------------------------- menu

pub fn show_menu(main: HWND, idle: bool, recording: bool, paused: bool, pause_label: &str) {
    unsafe {
        hide_from(&MENU);
        let sc = theme::scale_primary();
        let mut cur = POINT::default();
        let _ = GetCursorPos(&mut cur);
        let mk = |id: usize, icon: Icon, label: &str, shortcut: &str, enabled: bool| Btn {
            id,
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            icon,
            enabled,
            label: label.into(),
            shortcut: shortcut.into(),
        };
        let mut btns = vec![
            mk(1, Icon::Record, "Start recording", "Win+Shift+R", idle),
            mk(2, Icon::Pause, "Pause", pause_label, recording),
            mk(3, Icon::Play, "Resume", pause_label, paused),
            mk(4, Icon::Stop, "Stop", "", recording || paused),
        ];
        btns.push(mk(5, Icon::Folder, "Open output folder", "", true));
        btns.push(mk(6, Icon::Power, "Quit", "", true));
        let sep_after = 3usize;

        let w_dip = theme::MENU_W;
        let h_dip =
            theme::MENU_PAD * 2.0 + theme::MENU_ITEM_H * btns.len() as f32 + theme::MENU_SEP_H;
        let cw = (w_dip * sc).round() as i32;
        let ch = (h_dip * sc).round() as i32;
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let mut x = cur.x + 12;
        let mut y = cur.y + 12;
        if x + cw > vx + vw {
            x = cur.x - cw - 4;
        }
        if y + ch > vy + vh {
            y = (cur.y - ch - 4).max(vy);
        }
        // row rects (window-relative, physical; + shadow margin)
        let mg = theme::SHADOW_PAD * sc;
        let mut yy = theme::MENU_PAD * sc + mg;
        for (i, b) in btns.iter_mut().enumerate() {
            b.x = theme::MENU_PAD * sc + mg;
            b.y = yy;
            b.w = w_dip * sc - theme::MENU_PAD * 2.0 * sc;
            b.h = theme::MENU_ITEM_H * sc;
            yy += theme::MENU_ITEM_H * sc;
            if i == sep_after {
                yy += theme::MENU_SEP_H * sc;
            }
        }
        let s = new_surface("SnapperMenu", KIND_MENU, main, x, y, cw, ch, sc, btns, sep_after);
        let Ok(s) = s else { return };
        repaint(&s);
        present(&s);
        *MENU.lock().unwrap() = Some(s);
        let hwnd = MENU.lock().unwrap().as_ref().unwrap().hwnd;
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = SetTimer(hwnd, FADE_TIMER, 16, None);
    }
}

pub fn hide_menu() {
    hide_from(&MENU);
}

// ---------------------------------------------------------------- drawing

unsafe fn rr(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect,
        radiusX: radius,
        radiusY: radius,
    }
}

unsafe fn draw_icon(s: &Surface, b: &Btn, cx: f32, cy: f32) {
    let sc = s.sc;
    let a = s.alpha;
    let Ok(rt) = s.rt.cast::<ID2D1RenderTarget>() else {
        return;
    };
    let base = if b.enabled { theme::text() } else { theme::disabled() };
    match b.icon {
        Icon::Record => {
            let br = rt.CreateSolidColorBrush(&theme::accent(), None).unwrap();
            br.SetOpacity(a);
            rt.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F { x: cx, y: cy },
                    radiusX: 6.0 * sc,
                    radiusY: 6.0 * sc,
                },
                &br,
            );
        }
        Icon::Stop => {
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let r = 5.5 * sc;
            rt.FillRoundedRectangle(
                &rr(
                    D2D_RECT_F {
                        left: cx - r,
                        top: cy - r,
                        right: cx + r,
                        bottom: cy + r,
                    },
                    1.5 * sc,
                ),
                &br,
            );
        }
        Icon::Pause => {
            // two vertical bars (geometry — no font dependency)
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let bw = 2.6 * sc;
            let h = 12.0 * sc;
            for dx in [-4.6f32, 4.6] {
                let x = cx + dx * sc;
                rt.FillRoundedRectangle(
                    &rr(
                        D2D_RECT_F {
                            left: x - bw / 2.0,
                            top: cy - h / 2.0,
                            right: x + bw / 2.0,
                            bottom: cy + h / 2.0,
                        },
                        1.0 * sc,
                    ),
                    &br,
                );
            }
        }
        Icon::Play => {
            // right-pointing filled triangle (geometry — no font dependency)
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            if let Ok(pg) = d2d_factory().CreatePathGeometry() {
                if let Ok(sink) = pg.Open() {
                    sink.BeginFigure(
                        D2D_POINT_2F {
                            x: cx - 5.0 * sc,
                            y: cy - 7.0 * sc,
                        },
                        D2D1_FIGURE_BEGIN_FILLED,
                    );
                    sink.AddLines(&[
                        D2D_POINT_2F {
                            x: cx - 5.0 * sc,
                            y: cy + 7.0 * sc,
                        },
                        D2D_POINT_2F {
                            x: cx + 7.0 * sc,
                            y: cy,
                        },
                    ]);
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    let _ = sink.Close();
                    rt.FillGeometry(&pg, &br, None::<&ID2D1Brush>);
                }
            }
        }
        Icon::Camera => {
            // outline camera: body + viewfinder bump + lens
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let w = 1.5 * sc;
            rt.DrawRoundedRectangle(
                &rr(
                    D2D_RECT_F {
                        left: cx - 8.0 * sc,
                        top: cy - 4.5 * sc,
                        right: cx + 8.0 * sc,
                        bottom: cy + 6.5 * sc,
                    },
                    2.0 * sc,
                ),
                &br,
                w,
                None,
            );
            rt.DrawRoundedRectangle(
                &rr(
                    D2D_RECT_F {
                        left: cx - 3.5 * sc,
                        top: cy - 7.0 * sc,
                        right: cx + 0.5 * sc,
                        bottom: cy - 4.5 * sc,
                    },
                    1.0 * sc,
                ),
                &br,
                w,
                None,
            );
            rt.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F { x: cx, y: cy + 1.0 * sc },
                    radiusX: 2.8 * sc,
                    radiusY: 2.8 * sc,
                },
                &br,
                w,
                None,
            );
        }
        Icon::Close => {
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let w = 1.8 * sc;
            let d = 5.5 * sc;
            rt.DrawLine(
                D2D_POINT_2F { x: cx - d, y: cy - d },
                D2D_POINT_2F { x: cx + d, y: cy + d },
                &br,
                w,
                None,
            );
            rt.DrawLine(
                D2D_POINT_2F { x: cx - d, y: cy + d },
                D2D_POINT_2F { x: cx + d, y: cy - d },
                &br,
                w,
                None,
            );
        }
        Icon::Folder => {
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let w = 1.5 * sc;
            rt.DrawRoundedRectangle(
                &rr(
                    D2D_RECT_F {
                        left: cx - 8.0 * sc,
                        top: cy - 4.0 * sc,
                        right: cx + 8.0 * sc,
                        bottom: cy + 6.5 * sc,
                    },
                    1.5 * sc,
                ),
                &br,
                w,
                None,
            );
            // tab
            rt.DrawLine(
                D2D_POINT_2F {
                    x: cx - 8.0 * sc,
                    y: cy - 4.0 * sc,
                },
                D2D_POINT_2F {
                    x: cx - 4.0 * sc,
                    y: cy - 6.5 * sc,
                },
                &br,
                w,
                None,
            );
            rt.DrawLine(
                D2D_POINT_2F {
                    x: cx - 4.0 * sc,
                    y: cy - 6.5 * sc,
                },
                D2D_POINT_2F {
                    x: cx + 1.0 * sc,
                    y: cy - 6.5 * sc,
                },
                &br,
                w,
                None,
            );
        }
        Icon::Power => {
            let br = rt.CreateSolidColorBrush(&base, None).unwrap();
            br.SetOpacity(a);
            let w = 1.5 * sc;
            rt.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F { x: cx, y: cy + 1.0 * sc },
                    radiusX: 6.0 * sc,
                    radiusY: 6.0 * sc,
                },
                &br,
                w,
                None,
            );
            rt.DrawLine(
                D2D_POINT_2F {
                    x: cx,
                    y: cy - 7.5 * sc,
                },
                D2D_POINT_2F {
                    x: cx,
                    y: cy - 1.0 * sc,
                },
                &br,
                w,
                None,
            );
        }
    }
}

unsafe fn repaint(s: &Surface) {
    let n = (s.w as usize) * (s.h as usize);
    std::slice::from_raw_parts_mut(s.bits, n).fill(0);
    let _ = s.rt.BindDC(s.dc, &RECT { left: 0, top: 0, right: s.w, bottom: s.h });
    let Ok(rt) = s.rt.cast::<ID2D1RenderTarget>() else {
        return;
    };
    let _ = rt.BeginDraw();
    let sc = s.sc;
    let a = s.alpha;
    let m = theme::SHADOW_PAD * sc;
    let capsule = D2D_RECT_F {
        left: m,
        top: m,
        right: s.w as f32 - m,
        bottom: s.h as f32 - m,
    };
    // soft shadow: concentric rounded rects
    let sh = rt.CreateSolidColorBrush(&theme::col(0, 0, 0, 1.0), None).unwrap();
    for (inf, op) in [(10.0f32, 0.05f32), (7.0, 0.07), (4.0, 0.09), (1.5, 0.12)] {
        let r = D2D_RECT_F {
            left: capsule.left - inf * sc,
            top: capsule.top - inf * sc,
            right: capsule.right + inf * sc,
            bottom: capsule.bottom + inf * sc,
        };
        let rad = if s.kind == KIND_MENU {
            10.0 * sc + inf * sc
        } else {
            theme::BAR_H * sc / 2.0 + inf * sc
        };
        sh.SetOpacity(op * a);
        rt.FillRoundedRectangle(&rr(r, rad), &sh);
    }
    // capsule surface + hairline
    let rad = if s.kind == KIND_MENU {
        10.0 * sc
    } else {
        theme::BAR_H * sc / 2.0
    };
    let br = rt.CreateSolidColorBrush(&theme::surface(), None).unwrap();
    br.SetOpacity(a);
    rt.FillRoundedRectangle(&rr(capsule, rad), &br);
    let hl = rt.CreateSolidColorBrush(&theme::hairline(), None).unwrap();
    hl.SetOpacity(a);
    rt.DrawRoundedRectangle(&rr(capsule, rad), &hl, 1.0, None);

    // buttons
    for (i, b) in s.btns.iter().enumerate() {
        let hovered = s.hover == i as isize;
        if hovered && b.enabled {
            let hb = rt.CreateSolidColorBrush(&theme::hover(), None).unwrap();
            hb.SetOpacity(a);
            if s.kind == KIND_MENU {
                let inset = 3.0 * sc;
                rt.FillRoundedRectangle(
                    &rr(
                        D2D_RECT_F {
                            left: b.x + inset,
                            top: b.y + inset,
                            right: b.x + b.w - inset,
                            bottom: b.y + b.h - inset,
                        },
                        6.0 * sc,
                    ),
                    &hb,
                );
            } else {
                rt.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F {
                            x: b.x + b.w / 2.0,
                            y: b.y + b.h / 2.0,
                        },
                        radiusX: theme::BTN * sc / 2.0,
                        radiusY: theme::BTN * sc / 2.0,
                    },
                    &hb,
                );
            }
        }
        let cx = b.x + b.w / 2.0;
        let cy = b.y + b.h / 2.0;
        if s.kind == KIND_MENU {
            draw_icon(s, b, b.x + 16.0 * sc, cy);
            let f = &ui_format(theme::FONT * sc);
            let tb = rt
                .CreateSolidColorBrush(&if b.enabled { theme::text() } else { theme::disabled() }, None)
                .unwrap();
            tb.SetOpacity(a);
            let lw = u16s(&b.label);
            rt.DrawText(
                &lw,
                f,
                &D2D_RECT_F {
                    left: b.x + 36.0 * sc,
                    top: b.y,
                    right: b.x + b.w - (theme::TIMER_W + 8.0) * sc,
                    bottom: b.y + b.h,
                },
                &tb,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            if !b.shortcut.is_empty() {
                let sb = rt.CreateSolidColorBrush(&theme::text2(), None).unwrap();
                sb.SetOpacity(a);
                let sf = &ui_format((theme::FONT - 1.0) * sc);
                let _ = sf.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
                let sw = u16s(&b.shortcut);
                rt.DrawText(
                    &sw,
                    sf,
                    &D2D_RECT_F {
                        left: b.x + b.w - (theme::TIMER_W + 8.0) * sc,
                        top: b.y,
                        right: b.x + b.w - 8.0 * sc,
                        bottom: b.y + b.h,
                    },
                    &sb,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        } else {
            draw_icon(s, b, cx, cy);
        }
        if i == s.sep_after {
            let sl = rt.CreateSolidColorBrush(&theme::hairline(), None).unwrap();
            sl.SetOpacity(a);
            let y = b.y + b.h + theme::MENU_SEP_H * sc / 2.0;
            rt.DrawLine(
                D2D_POINT_2F {
                    x: b.x + 8.0 * sc,
                    y,
                },
                D2D_POINT_2F {
                    x: b.x + b.w - 8.0 * sc,
                    y,
                },
                &sl,
                1.0,
                None,
            );
        }
    }
    // ctl bar live timer
    if s.kind == KIND_CTL && !s.timer.is_empty() {
        let colr = if s.paused {
            theme::text2()
        } else {
            theme::text()
        };
        let tb = rt.CreateSolidColorBrush(&colr, None).unwrap();
        tb.SetOpacity(a);
        let f = &ui_format(theme::FONT * sc);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
        let tw = u16s(&s.timer);
        rt.DrawText(
            &tw,
            f,
            &D2D_RECT_F {
                left: s.w as f32 - m - (theme::TIMER_W + 4.0) * sc,
                top: m,
                right: s.w as f32 - m - 4.0 * sc,
                bottom: s.h as f32 - m,
            },
            &tb,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
    // default-action accent hairline (keyboard affordance for Enter)
    if s.kind == KIND_ACTION {
        let b = &s.btns[0];
        let ab = rt.CreateSolidColorBrush(&theme::accent(), None).unwrap();
        ab.SetOpacity(0.8 * a);
        rt.DrawEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: b.x + b.w / 2.0,
                    y: b.y + b.h / 2.0,
                },
                radiusX: theme::BTN * sc / 2.0,
                radiusY: theme::BTN * sc / 2.0,
            },
            &ab,
            1.0,
            None,
        );
    }
    let _ = s.rt.EndDraw(None, None);
}

unsafe fn present(s: &Surface) {
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let pos = POINT { x: s.x, y: s.y };
    let src = POINT { x: 0, y: 0 };
    let size = SIZE { cx: s.w, cy: s.h };
    if let Err(e) = UpdateLayeredWindow(
        s.hwnd,
        HDC::default(),
        Some(&pos),
        Some(&size),
        s.dc,
        Some(&src),
        COLORREF(0),
        Some(&bf),
        ULW_ALPHA,
    ) {
        eprintln!("[ui] UpdateLayeredWindow failed: {e}");
    }
}

fn hit(s: &Surface, px: i32, py: i32) -> isize {
    for (i, b) in s.btns.iter().enumerate() {
        if !b.enabled {
            continue;
        }
        let (px, py) = (px as f32, py as f32);
        if s.kind == KIND_MENU {
            if px >= b.x && px < b.x + b.w && py >= b.y && py < b.y + b.h {
                return i as isize;
            }
        } else {
            let cx = b.x + b.w / 2.0;
            let cy = b.y + b.h / 2.0;
            let dx = px - cx;
            let dy = py - cy;
            if (dx * dx + dy * dy).sqrt() <= theme::BTN * s.sc / 2.0 {
                return i as isize;
            }
        }
    }
    -1
}

unsafe fn do_click(s: &Surface, id: usize) {
    let msg = match s.kind {
        KIND_ACTION => WM_APP_ACTION,
        KIND_CTL => WM_APP_CTL,
        _ => WM_APP_MENU,
    };
    let _ = PostMessageW(s.main, msg, WPARAM(id), LPARAM(0));
}

fn get_x(lp: LPARAM) -> i32 {
    ((lp.0 as u32) as u16) as i16 as i32
}
fn get_y(lp: LPARAM) -> i32 {
    (((lp.0 as u32) >> 16) as u16) as i16 as i32
}

unsafe fn fade_tick(slot: &Mutex<Option<Surface>>, hwnd: HWND) -> LRESULT {
    let mut g = slot.lock().unwrap();
    if let Some(s) = g.as_mut() {
        s.alpha = (s.alpha + 0.2).min(1.0);
        repaint(s);
        present(s);
        if s.alpha >= 1.0 {
            let _ = KillTimer(hwnd, FADE_TIMER);
        }
    }
    LRESULT(0)
}

unsafe extern "system" fn capsule_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut g = CAPSULE.lock().unwrap();
            if let Some(s) = g.as_mut() {
                let h = hit(s, get_x(lp), get_y(lp));
                if h != s.hover {
                    s.hover = h;
                    repaint(s);
                    present(s);
                }
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let mut g = CAPSULE.lock().unwrap();
            if let Some(s) = g.as_mut() {
                if s.hover != -1 {
                    s.hover = -1;
                    repaint(s);
                    present(s);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut g = CAPSULE.lock().unwrap();
            if let Some(s) = g.as_mut() {
                let h = hit(s, get_x(lp), get_y(lp));
                if h >= 0 {
                    let id = s.btns[h as usize].id;
                    let kind = s.kind;
                    do_click(s, id);
                    if kind == KIND_ACTION {
                        drop(g);
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wp.0 as u32;
            let mut g = CAPSULE.lock().unwrap();
            if let Some(s) = g.as_mut() {
                if s.kind == KIND_ACTION && key == VK_ESCAPE.0 as u32 {
                    do_click(s, 3);
                    drop(g);
                    let _ = DestroyWindow(hwnd);
                } else if s.kind == KIND_ACTION && key == VK_RETURN.0 as u32 {
                    do_click(s, 1);
                    drop(g);
                    let _ = DestroyWindow(hwnd);
                } else if s.kind == KIND_CTL && key == VK_SPACE.0 as u32 {
                    do_click(s, 1);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == FADE_TIMER => fade_tick(&CAPSULE, hwnd),
        WM_SNAP_CLICK => {
            let id = wp.0;
            let g = CAPSULE.lock().unwrap();
            if let Some(s) = g.as_ref() {
                do_click(s, id);
                if s.kind == KIND_ACTION {
                    drop(g);
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut g = CAPSULE.lock().unwrap();
            if let Some(s) = g.take() {
                let _ = DeleteObject(HGDIOBJ(s.dib.0));
                let _ = DeleteDC(s.dc);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut g = MENU.lock().unwrap();
            if let Some(s) = g.as_mut() {
                let h = hit(s, get_x(lp), get_y(lp));
                if h != s.hover {
                    s.hover = h;
                    repaint(s);
                    present(s);
                }
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let mut g = MENU.lock().unwrap();
            if let Some(s) = g.as_mut() {
                if s.hover != -1 {
                    s.hover = -1;
                    repaint(s);
                    present(s);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut g = MENU.lock().unwrap();
            if let Some(s) = g.as_mut() {
                let h = hit(s, get_x(lp), get_y(lp));
                if h >= 0 {
                    let id = s.btns[h as usize].id;
                    do_click(s, id);
                    drop(g);
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wp.0 as u32;
            if key == VK_ESCAPE.0 as u32 {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            let mut g = MENU.lock().unwrap();
            if let Some(s) = g.as_mut() {
                if key == VK_UP.0 as u32 || key == VK_DOWN.0 as u32 {
                    let dir = if key == VK_DOWN.0 as u32 { 1isize } else { -1isize };
                    let n = s.btns.len() as isize;
                    let mut i = s.hover;
                    for _ in 0..n {
                        i = (i + n + dir) % n;
                        if s.btns[i as usize].enabled {
                            break;
                        }
                    }
                    if s.btns[i as usize].enabled {
                        s.hover = i;
                        repaint(s);
                        present(s);
                    }
                } else if key == VK_RETURN.0 as u32 && s.hover >= 0 {
                    let id = s.btns[s.hover as usize].id;
                    do_click(s, id);
                    drop(g);
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == FADE_TIMER => fade_tick(&MENU, hwnd),
        WM_KILLFOCUS => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut g = MENU.lock().unwrap();
            if let Some(s) = g.take() {
                let _ = DeleteObject(HGDIOBJ(s.dib.0));
                let _ = DeleteDC(s.dc);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------- tray icon

static ICON_RGBA: OnceLock<Option<(u32, u32, Vec<u8>)>> = OnceLock::new();

/// Decodes assets/icon.png (RGBA8) once; None if the asset is missing.
fn icon_rgba() -> Option<&'static (u32, u32, Vec<u8>)> {
    ICON_RGBA
        .get_or_init(|| {
            let bytes = include_bytes!("../assets/icon-128.png");
            let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
            let Ok(mut reader) = decoder.read_info() else {
                return None;
            };
            let w = reader.info().width;
            let h = reader.info().height;
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            if reader
                .next_frame(&mut buf)
                .map_err(|_| ())
                .and_then(|_| {
                    if reader.info().color_type != png::ColorType::Rgba {
                        return Err(());
                    }
                    Ok(())
                })
                .is_ok()
            {
                Some((w, h, buf))
            } else {
                None
            }
        })
        .as_ref()
}

fn mix_u8(a: u8, b: u8, f: u32) -> u8 {
    (a as u32 * (1000 - f) / 1000 + b as u32 * f / 1000) as u8
}

/// High-quality downscale of RGBA8: progressive 2x2 box halving, then bilinear.
fn hscale(src: &[u8], sw: u32, sh: u32, w: u32, h: u32) -> Vec<u8> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut cur = src.to_vec();
    let mut cw = sw;
    let mut ch = sh;
    while cw / 2 >= w && ch / 2 >= h && cw > 1 && ch > 1 {
        let nw = (cw + 1) / 2;
        let nh = (ch + 1) / 2;
        let mut next = vec![0u8; (nw as usize) * (nh as usize) * 4];
        for y in 0..nh {
            for x in 0..nw {
                let mut acc = [0u32; 4];
                let mut cnt = 0u32;
                for dy in 0..2 {
                    let sy = y * 2 + dy;
                    if sy >= ch {
                        continue;
                    }
                    for dx in 0..2 {
                        let sx = x * 2 + dx;
                        if sx >= cw {
                            continue;
                        }
                        let o = (sy as usize * cw as usize + sx as usize) * 4;
                        for c in 0..4 {
                            acc[c] += cur[o + c] as u32;
                        }
                        cnt += 1;
                    }
                }
                let o = (y as usize * nw as usize + x as usize) * 4;
                for c in 0..4 {
                    next[o + c] = (acc[c] / cnt) as u8;
                }
            }
        }
        cur = next;
        cw = nw;
        ch = nh;
    }
    let mut dst = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        for x in 0..w {
            let fx = ((x as f64 + 0.5) * cw as f64 / w as f64) - 0.5;
            let fy = ((y as f64 + 0.5) * ch as f64 / h as f64) - 0.5;
            let x0 = fx.floor().max(0.0) as u32;
            let y0 = fy.floor().max(0.0) as u32;
            let x1 = (x0 + 1).min(cw - 1);
            let y1 = (y0 + 1).min(ch - 1);
            let tx = ((fx - x0 as f64) * 1000.0).clamp(0.0, 1000.0) as u32;
            let ty = ((fy - y0 as f64) * 1000.0).clamp(0.0, 1000.0) as u32;
            let ro = |yy: u32, xx: u32| (yy as usize * cw as usize + xx as usize) * 4;
            let (a0, b0, c0, d0) = (ro(y0, x0), ro(y0, x1), ro(y1, x0), ro(y1, x1));
            let o = (y as usize * w as usize + x as usize) * 4;
            for c in 0..4 {
                let top = mix_u8(cur[a0 + c], cur[b0 + c], tx);
                let bot = mix_u8(cur[c0 + c], cur[d0 + c], tx);
                dst[o + c] = mix_u8(top, bot, ty);
            }
        }
    }
    dst
}

/// Premultiplies `rgba` (size x size) into the BGRA DIB `bits`.
unsafe fn blit_rgba(bits: &mut [u32], rgba: &[u8], size: usize) {
    for i in 0..size * size {
        let o = i * 4;
        let r = rgba[o] as u32;
        let g = rgba[o + 1] as u32;
        let b = rgba[o + 2] as u32;
        let a = rgba[o + 3] as u32;
        bits[i] = (a << 24)
            | ((b * a / 255) << 16)
            | ((g * a / 255) << 8)
            | (r * a / 255);
    }
}

/// State dot painted at the bottom-right of the tray icon.
unsafe fn draw_state_dot_gdi(dc: HDC, size: i32, state: u8) {
    if state == 0 {
        return;
    }
    let c = if state == 1 { theme::accent() } else { theme::amber() };
    let rf = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let ring = (size as f32 * 0.08).max(1.0) as i32;
    let r = (size as f32 * 0.26) as i32;
    let cx = size - r - (size as f32 * 0.04) as i32 - 1;
    let cy = size - r - (size as f32 * 0.04) as i32 - 1;
    let hp = CreatePen(PS_SOLID, ring, COLORREF(0x000000));
    let obp = SelectObject(dc, hp);
    let hb = CreateSolidBrush(COLORREF(
        (rf(c.b) as u32) << 16 | (rf(c.g) as u32) << 8 | rf(c.r) as u32,
    ));
    let obb = SelectObject(dc, hb);
    let _ = Ellipse(dc, cx - r, cy - r, cx + r, cy + r);
    SelectObject(dc, obb);
    SelectObject(dc, obp);
    let _ = DeleteObject(hb);
    let _ = DeleteObject(hp);
}

/// Renders the tray glyph (app icon + state dot) at `size` physical px.
/// Returns RGBA8 (un-premultiplied) for tray_icon::Icon::from_rgba.
pub fn render_tray_icon(state: u8, size: i32) -> Vec<u8> {
    if let Some((iw, ih, src)) = icon_rgba() {
        let scaled = hscale(src, *iw, *ih, size as u32, size as u32);
        unsafe {
            let (dib, dc, bits, _dcr) = make_surface(size, size, 1.0).expect("tray dib");
            let n = (size as usize) * (size as usize);
            std::slice::from_raw_parts_mut(bits, n).fill(0);
            blit_rgba(std::slice::from_raw_parts_mut(bits, n), &scaled, size as usize);
            draw_state_dot_gdi(dc, size, state);
            let buf = std::slice::from_raw_parts(bits, n);
            let mut rgba = vec![0u8; n * 4];
            for i in 0..n {
                let b = buf[i];
                let a = (b >> 24) as u32;
                if a == 0 {
                    continue;
                }
                let o = i * 4;
                let un = |c: u32| (((c & 0xFF) * 255) / a).min(255) as u8;
                rgba[o] = un(b & 0xFF); // R
                rgba[o + 1] = un((b >> 8) & 0xFF); // G
                rgba[o + 2] = un((b >> 16) & 0xFF); // B
                rgba[o + 3] = a as u8;
            }
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(dc);
            rgba
        }
    } else {
        render_tray_icon_glyph(state, size)
    }
}

/// Fallback when assets/icon.png is unavailable: MDL2 camera glyph + state dot.
fn render_tray_icon_glyph(state: u8, size: i32) -> Vec<u8> {
    unsafe {
        let (dib, dc, bits, dcr) = make_surface(size, size, 1.0).expect("tray dib");
        let Ok(rt) = dcr.cast::<ID2D1RenderTarget>() else {
            return Vec::new();
        };
        let n = (size as usize) * (size as usize);
        std::slice::from_raw_parts_mut(bits, n).fill(0);
        let _ = rt.BeginDraw();
        let f = &glyph_format(size as f32 * 0.72);
        let gw = u16s(&String::from(theme::GL_CAMERA));
        let rect = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: size as f32,
            bottom: size as f32,
        };
        // dark halo for contrast on light taskbars, then white glyph
        let halo = rt.CreateSolidColorBrush(&theme::col(0, 0, 0, 0.55), None).unwrap();
        let halo_rect = D2D_RECT_F {
            left: 0.0,
            top: size as f32 * 0.04,
            right: size as f32,
            bottom: size as f32 * 1.04,
        };
        rt.DrawText(
            &gw,
            f,
            &halo_rect,
            &halo,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        let wb = rt.CreateSolidColorBrush(&theme::col(255, 255, 255, 1.0), None).unwrap();
        rt.DrawText(
            &gw,
            f,
            &rect,
            &wb,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        if state > 0 {
            let c = if state == 1 { theme::accent() } else { theme::amber() };
            let db = rt.CreateSolidColorBrush(&c, None).unwrap();
            let r = size as f32 * 0.26;
            let cx = size as f32 - r - size as f32 * 0.04;
            let cy = size as f32 - r - size as f32 * 0.04;
            let ell = D2D1_ELLIPSE {
                point: D2D_POINT_2F { x: cx, y: cy },
                radiusX: r,
                radiusY: r,
            };
            let ring = rt.CreateSolidColorBrush(&theme::col(0, 0, 0, 0.6), None).unwrap();
            rt.DrawEllipse(&ell, &ring, (size as f32 * 0.08).max(1.0), None);
            rt.FillEllipse(&ell, &db);
        }
        let _ = dcr.EndDraw(None, None);
        let buf = std::slice::from_raw_parts(bits, n);
        let mut rgba = vec![0u8; n * 4];
        for i in 0..n {
            let b = buf[i];
            let a = (b >> 24) as u32;
            let o = i * 4;
            if a == 0 {
                continue;
            }
            let un = |c: u32| (((c & 0xFF) * 255) / a).min(255) as u8;
            rgba[o] = un(b & 0xFF); // R
            rgba[o + 1] = un((b >> 8) & 0xFF); // G
            rgba[o + 2] = un((b >> 16) & 0xFF); // B
            rgba[o + 3] = a as u8;
        }
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(dc);
        rgba
    }
}
