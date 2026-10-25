// Design tokens: dark minimal capsule system (see plan.md §2).
// All sizes are logical px (DIPs); multiply by monitor scale at draw time.
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint,
    MonitorFromRect,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

pub fn col(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

pub fn surface() -> D2D1_COLOR_F {
    col(14, 14, 17, 0.78)
}
pub fn hairline() -> D2D1_COLOR_F {
    col(255, 255, 255, 0.10)
}
pub fn text() -> D2D1_COLOR_F {
    col(255, 255, 255, 0.90)
}
pub fn text2() -> D2D1_COLOR_F {
    col(255, 255, 255, 0.55)
}
pub fn disabled() -> D2D1_COLOR_F {
    col(255, 255, 255, 0.30)
}
pub fn accent() -> D2D1_COLOR_F {
    col(255, 69, 58, 1.0)
}
pub fn amber() -> D2D1_COLOR_F {
    col(255, 178, 51, 1.0)
}
pub fn hover() -> D2D1_COLOR_F {
    col(255, 255, 255, 0.10)
}
pub fn dim() -> D2D1_COLOR_F {
    col(0, 0, 0, 0.33)
}

pub const BAR_H: f32 = 36.0;
pub const PAD: f32 = 8.0;
pub const GAP: f32 = 4.0;
pub const BTN: f32 = 32.0;
pub const SHADOW_PAD: f32 = 14.0;
pub const TIMER_W: f32 = 52.0;
pub const MENU_W: f32 = 280.0;
pub const MENU_ITEM_H: f32 = 30.0;
pub const MENU_PAD: f32 = 6.0;
pub const MENU_SEP_H: f32 = 9.0;
pub const FONT: f32 = 13.0;

// Segoe MDL2 Assets glyph (tray icon fallback camera; present on Win10 + Win11).
// Only used if the embedded assets\icon-128.png failed to decode; the exe icon is
// assets\icon.ico (embedded by build.rs via app.rc).
pub const GL_CAMERA: char = '\u{E722}';

pub fn scale_for_rect(r: RECT) -> f32 {
    unsafe { dpi_scale(MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST)) }
}

pub fn scale_primary() -> f32 {
    unsafe { dpi_scale(MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)) }
}

fn dpi_scale(m: HMONITOR) -> f32 {
    unsafe {
        let mut x = 0u32;
        let mut y = 0u32;
        if GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x > 0 {
            x as f32 / 96.0
        } else {
            1.0
        }
    }
}
