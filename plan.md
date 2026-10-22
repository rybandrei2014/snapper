# Snapper — Visual Polish Plan (2026-10-22)

> **STATUS 2026-10-22: P0–P4 implemented + bugfix round** (D2D capsules for both bars + tray
> menu, overlay restyle + size badge + fade, glowing recording border, AA state tray icons,
> DPI-scaled). Bugfixes: capsule buttons were capsule-relative but drawn in window coords that
> include the shadow margin → added `m=SHADOW_PAD*sc` offset (fixes misalignment); MDL2 glyphs
> render as tofu through the DWrite path → all bar/menu icons are now geometry (record/stop/
> pause/play/camera/close/folder/power), tray camera glyph kept; recording perceived-start delay
> → border + ctl bar shown before ffmpeg spawn, `Segment.start = t0` at spawn, health check
> 400→250 ms (border appears in ~37 ms). Deviations: tray-icon 0.19 has no `show_notification`
> → kept tooltips; acrylic blur (stretch) not done; hover tooltips on icon-only buttons not yet
> done. Verified: `e2e_test.ps1` (6.96 s MP4), `test_shot.ps1`, `test_sync.ps1` (A/V ~33 ms),
> `test_sync2.ps1` (pause trim: event gap 4.007 s vs expected 4 s), `visual_test.ps1` screenshots
> of chooser/rec/pause states at 100% DPI.
>
> Replaces the original v1 implementation plan (all milestones through M2 done; the original
> document is preserved in git history, commit `76593f5`). Unimplemented v1 leftovers are
> carried over in §8.
>
> **Design brief (v2):** deliberately NOT Windows-native. Target aesthetic: dark, minimal,
> "pro tool" — closer to Raycast / Linear / macOS menu-bar utilities than to Win32 chrome.

## 1. Problem Inventory (what looks old, and where)

| Element | Current implementation | Why it looks like a legacy app |
|---|---|---|
| Record/Screenshot bar | Plain `WS_POPUP` with `COLOR_BTNFACE` background (`overlay.rs:109`) + classic `BUTTON` child controls (`overlay.rs:448-470`) | System gray face, no rounding, no shadow, no dark theme, fixed 224×44 px |
| Pause/Stop control bar | Same pattern (`overlay.rs:117-124`, `overlay.rs:514-578`) | Same; fixed 168×36 px |
| Fonts | `GetStockObject(DEFAULT_GUI_FONT)` (`overlay.rs:447`, `overlay.rs:548`) | **Biggest tell.** Bitmap "MS Sans Serif" — aliased, tiny, never scales with DPI |
| Selection overlay | Fullscreen dim alpha 128 (`overlay.rs:26`), 2 px hard red border drawn *inside* the selection (`overlay.rs:234-243`) | Too dark, harsh square border, no size readout, no guides, no fade-in |
| Recording border | 3 px hard red rectangle (`overlay.rs:376-381`) | Square, no outline/contrast strategy, no animation |
| Tray icon | Procedural 32×32 red circle, `sqrt(dx²+dy²) <= 13.5` (`main.rs:82-99`) | Hard-edged (no anti-aliasing) → jaggy at every DPI; same icon in every state |
| Tray context menu | Plain text `muda` items (`main.rs:169-185`) | Native HMENU: classic gray on Win10, plain/iconless on Win11 |
| Motion | None anywhere | Modern UIs fade/slide in; everything here pops instantly |
| DPI | Process is PerMonitorV2 (`main.rs:103`) but bars use hardcoded physical pixel sizes | On 150%/200% displays bars are tiny and text is unreadable |

Root causes, ranked by perceived impact:
1. **Bitmap stock font** — instantly reads as Windows 95/XP.
2. **System-color chrome** (COLOR_BTNFACE gray) instead of a designed surface.
3. **No depth**: no rounding, no shadow, no hairline strokes.
4. **Jagged, stateless tray icon**.
5. **No DPI scaling, no motion, no translucency.**

## 2. Design Direction — "dark minimal capsule"

Think: a floating macOS menu-bar utility. Small translucent capsules, hairline strokes,
icon-only controls, one accent color, quiet motion. No system colors, no native widgets.

Design tokens (new `src/theme.rs`, all values in **logical px (DIPs)**, × monitor scale):

```
capsule:      radius = height/2 (full pill); bars 36 DIP tall
surface:      #0E0E11 @ 78% alpha (near-black, cool)
hairline:     #FFFFFF @ 10%, 1px inner stroke (the "glass edge")
shadow:       black 40% @ 24px blur falloff (drawn procedurally)
accent:       #c4413a (soft red, macOS-ish) — used ONLY for: record dot, recording border
text:         #FFFFFFE6; secondary #FFFFFF99
hover:        #FFFFFF @ 10% pill behind icon; pressed @ 16%
font:         Segoe UI Variable Text → fallback Segoe UI; 13px; NO bold except live timer
icons:        Segoe MDL2 / Segoe Fluent glyphs, 16px, stroke-only look:
              record ●, camera, pause, stop, play, folder, power (verify codepoints at impl)
spacing:      icon buttons 32×32 DIP, 4px gaps, 8px capsule padding; NO text labels on bars
selection:    dim #000000 @ 33%; selection edge = 1px #FFFFFF @ 85% drawn OUTSIDE the region
size badge:   small capsule, 12px text "1280 × 720", follows cursor, 6px offset
motion:       150ms fade + 4px slide-up (ease-out) for bars; 160ms fade for overlay;
              recording border: static (no pulse) — minimalism; timer text is the only "alive" thing
```

Icon-only buttons hurt discoverability → compensate with hover tooltips (custom-drawn mini
capsule) and keyboard accelerators (Enter/Space/Esc).

## 3. Rendering Options

| Option | Approach | Pros | Cons | Verdict |
|---|---|---|---|---|
| A | GDI DIB + `UpdateLayeredWindow` (current pipeline, `overlay.rs:154-179`) | Already proven in-repo, zero deps | Hand-rolled AA for rounded pills/gradients is painful; GDI text is not premultiplied, mediocre at fractional DPI | Only for the fullscreen dim fill |
| B | **Direct2D + DirectWrite → WIC bitmap → `UpdateLayeredWindow`** | OS-provided (no crates beyond `windows` features), perfect AA geometry (rounded rects, strokes, radial-gradient glow), crisp text at any DPI, premultiplied by default — exactly what "slick" requires | ~1 day interop setup (`ID2D1Factory`, `ID2D1HwndRenderTarget` or WIC→DIB) | ✅ **Chosen** for bars, badges, menus, tray icons |
| C | egui/Slint | Fast to style | +1.5–3 MB, transparency quirks, two UI stacks | Only for the future settings window (P5) |
| D | XAML/WinUI | Native modern | Runtime dep, Windows-look ceiling — opposite of the brief | No |

**Real backdrop blur (the "acrylic" differentiator):** on Win11, non-layered windows can get
`DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_TRANSIENT_WINDOW)` → true acrylic;
Win10 fallback: `DwmEnableBlurBehindWindow` (frosted) → flat translucent if that fails.
Layered windows can't have backdrops, so bars in blur mode use a normal `WS_POPUP` + D2D
`IDCompositionTarget`. Treat as **P1 stretch goal** — flat 78% capsule already looks modern;
blur is polish on top. Decision: ship flat first, try blur behind a config flag.

## 4. Work Plan

### P0 — Quick wins (≈ half a day)
1. Fonts: `theme.rs::ui_font(dpi)` → `CreateFontW("Segoe UI Variable Text" → "Segoe UI")`;
   replace both `DEFAULT_GUI_FONT` uses (`overlay.rs:447`, `overlay.rs:548`).
2. Bar background: dark brush instead of `COLOR_BTNFACE`; `WM_CTLCOLORBTN`/`WM_ERASEBKGND`
   so buttons don't repaint gray (interim until P1 deletes the BUTTON children).
3. Tray icon: supersample (render 128², coverage AA, box-downsample to `SM_CXSMICON × DPI`).
4. Menu: `muda::MenuItem::set_icon` glyphs + shorter labels (interim until P4).
5. `tray.show_notification("Saved …")` for terminal events (`main.rs:431-447`, `main.rs:493-507`).

### P1 — Capsule bars, custom-drawn (≈ 2 days, the big one)
New `src/ui.rs`: D2D/DWrite render helpers (capsule, hairline, icon glyph, text, glow) +
one `FloatingCapsule` window type replacing both `SnapperAction` and `SnapperCtl`:
- `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`; render into WIC premultiplied BGRA →
  `UpdateLayeredWindow` (same present path as overlay).
- **Action bar**: capsule, icon-only: ⏺ record, 📷 screenshot, ✕ cancel (cancel = explicit affordance
  we currently lack outside Esc).
- **Control bar**: ⏸/▶ toggle, ⏹ stop, plus a live `mm:ss` timer in text token — the one dynamic
  element; timer also communicates paused state (dimmed + "⏸" glyph) better than a label swap.
- Hover/press pills via `WM_MOUSEMOVE` + `TrackMouseEvent` (leave → repaint); hit rects in state;
  click posts existing `WM_APP_ACTION`/`WM_APP_CTL` ids — state machine untouched.
- Keyboard: Enter = primary, Space = pause toggle, Esc unchanged; no focus rectangles — default
  button gets a 1px accent hairline instead.
- DPI: DIP tokens × `GetDpiForWindow`; `WM_DPICHANGED` re-renders.
- Stretch: acrylic backdrop mode (see §3).

### P2 — Overlay & recording border (≈ 1 day)
1. Dim 128 → ~85 alpha; selection border: 1 px white @ 85% drawn *outside* the region
   (`paint_overlay`, `overlay.rs:234-248`).
2. Live size badge (capsule, follows cursor) — closes the M1 gap.
3. Fade-in via `SetTimer` + `LWA_ALPHA` ramp (overlay); bars fade+slide in P1.
4. Recording border: 1.5 px accent + 2 px soft outer glow (alpha falloff in the DIB) instead of
   3 px hard red; no pulse.
5. Optional (ties to M4): window-edge snapping, 8 px magnet, hairline highlight on snap edges.

### P3 — Tray identity (≈ half a day)
- Thin-stroke aperture/camera glyph, **monochrome** (white + 15% dark halo → works on light and
  dark taskbars); states: idle glyph only · recording = glyph + accent dot · paused = glyph +
  amber dot. AA, sizes 16–48.
- `tray.set_icon()` on every `Mode` transition (`main.rs`).

### P4 — Custom tray menu (≈ 1–2 days, now default not optional)
Native HMENU contradicts the design brief (gray Win10 / plain Win11). Reuse P1's capsule renderer:
dark capsule popup at cursor, generous padding, pill hover, icon + label rows, separator hairline,
Esc/arrows/click-out dismiss. Keep `muda`'s `MenuEvent` ids as the action contract so `main.rs`
is untouched. (Native dark-menu hacks via uxtheme ordinals: forbidden — undocumented.)

### P5 — Settings window (deferred, pairs with v1-plan M3)
The one place where **Option C (egui or Slint)** earns its keep — real form controls, same tokens.
Capsules/overlay stay D2D.

## 5. Verification

- `e2e_test.ps1` / `test_shot.ps1` click **fixed button coordinates** — capsule geometry changes
  break them; update coordinates in the same commit (or read bar position from debug output).
- Manual matrix: 100/150/200% DPI, light + dark taskbar, Win10 + Win11; screenshot every surface.
- D2D check: text crisp at 125/150%; capsule edges smooth (zoom 400%); no GDI text fallback left.
- Acceptance: bars read as one designed system (same capsule/hairline/hover); Record/Screenshot
  and Pause/Stop e2e flows still pass; idle CPU unchanged (bars render only on state/hover change).

## 6. Risks

| Risk | Mitigation |
|---|---|
| D2D/windows-rs interop boilerplate | One-time `ui.rs` helper; fallback to GDI DIB (Option A) if it stalls — visual ceiling lower but plan survives |
| e2e scripts break on new geometry | Update coordinates same commit; keep hit rects stable |
| Icon-only buttons = poor discoverability | Hover tooltips (mini capsule) + accelerators + tray menu keeps text labels |
| WIC→`UpdateLayeredWindow` premultiplication mistakes | D2D/WIC are premultiplied natively; test with 50% surface over bright desktop first |
| Acrylic is Win11-only; blur-behind quirks on Win10 | Flat translucent default; blur behind config flag, per-OS fallback chain |
| Custom menu = keyboard nav/Dismiss/monitor-clamping all ours | Small item count (≤6); clamp to work area; Esc + click-outside; arrows optional v1.1 |
| Segoe Fluent Icons absent on Win10 | Fall back to Segoe MDL2 Assets (same glyphs) |

## 7. Suggested Commit Order

1. `theme.rs` + fonts + dark brush + AA tray icon + menu icons + notifications (P0)
2. `ui.rs` D2D capsule renderer + action bar (P1a) → control bar + timer (P1b) + e2e updates
3. Overlay dim/hairline/badge/fade + recording border glow (P2)
4. Tray state glyphs + `set_icon` wiring (P3)
5. Custom capsule tray menu (P4)
6. (stretch, flag-gated) acrylic backdrop mode

## 8. Carried over from v1 plan (still open)

Countdown timer · re-select last region (`Win+Shift+G`) · quality presets · settings window (P5) ·
installer + code signing · GIF export · window snapping (P2.5) · per-monitor DPI correctness
(P1 makes UI DPI-correct; capture-side DPI math still open).
