# AGENTS.md

Guidance for AI agents working in this repo. Read this before changing code.

## What this is

Snapper — a lightweight Windows region screen recorder in Rust. Tray-only app (no main window).
`Win+Shift+R` → dimmed fullscreen overlay → drag a rectangle → a capsule bar offers **Record**
(MP4) or **Screenshot** (PNG). Recording captures the region via `ffmpeg ddagrab` + system audio
(WASAPI loopback, optional mic), shows a red border and a Pause/Stop capsule bar, and supports
pause/resume (`Win+Alt+P`) with paused spans trimmed out on stop.

Platform: **Windows only**. Uses the `windows` crate (Win32/Direct2D/DirectWrite) + `tray-icon`.
No UI framework. ffmpeg is an external child process (must be on PATH or next to the exe).

## Build

```
cargo build              # debug  -> target\debug\snapper.exe
cargo build --release    # release -> target\release\snapper.exe (~0.78 MB, stripped + LTO)
```

**Portability is a hard requirement.** `.cargo/config.toml` sets
`rustflags = ["-C", "target-feature=+crt-static"]` for `x86_64-pc-windows-msvc`, so the CRT is
statically linked and the exe imports **only DLLs that ship with Windows 10/11** (d2d1, dwrite,
gdi32, user32, kernel32, ole32, oleaut32, shell32, ntdll + `api-ms-win-*` API-set stubs). No
VC++ redistributable, no installer, no bundled runtime. Do NOT remove that config — without it the
exe links `VCRUNTIME140.dll` + UCRT and breaks on clean machines. Verify with the PE import dump
(see commit history) if unsure. The only external dependency is **ffmpeg.exe** (a child process,
must be on PATH or next to the exe) — ship it alongside for a portable bundle.

`build.rs` compiles `app.rc` (embeds `assets\icon.ico` as the exe icon) with the Windows SDK
`rc.exe`; if the SDK isn't found it warns and ships the default icon. The tray icon is
`assets\icon-128.png`, embedded into the binary with `include_bytes!` and decoded at runtime (the
`png` crate); `assets\icon.png` is the 256px master (the size floor for `icon.ico`; regenerate
`icon.ico` + `icon-128.png` from it if the art changes).

There is no `cargo test` suite and no lints configured. Verify changes with the e2e scripts below.

## Release / CI

`.github/workflows/release.yml` (GitHub Actions, `windows-latest`): builds `--target
x86_64-pc-windows-msvc`, downloads a pinned ffmpeg build (BtbN win64-gpl), copies `snapper.exe` +
`ffmpeg.exe` + `ffprobe.exe` into a `dist/` folder, runs a **portability gate** (fails the build if
the exe imports `VCRUNTIME140` or `api-ms-win-crt-*`), zips it, and publishes a GitHub Release on
`v*` tags (also `workflow_dispatch` for manual runs). The e2e scripts are NOT run in CI (they need a
real desktop session).

### Changelog / release tagging

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) + Semantic Versioning.
Every user-visible change goes under `## [Unreleased]` (grouped `Added` / `Changed` / `Fixed`) as part
of the change itself — not as a separate "update the changelog" commit. Release notes for the GitHub
Release are taken from the matching `## [vX.Y.Z]` section by `release.yml`, so the section text is the
release body verbatim.

To cut a release (mirror `yt-mp3`):
1. Bump `version` in `Cargo.toml`.
2. In `CHANGELOG.md`: rename `## [Unreleased]` → `## [vX.Y.Z] - YYYY-MM-DD`, add a fresh empty
   `## [Unreleased]` above it, and update the compare links at the bottom
   (`[Unreleased]: .../compare/vX.Y.Z...HEAD`, `[vX.Y.Z]: .../compare/vPREV...vX.Y.Z`).
3. Commit, then `git tag vX.Y.Z && git push origin vX.Y.Z` — the tag triggers the release workflow.

Tags already on the repo: `v0.1.0`, `v0.2.0`.

## Testing

All tests are PowerShell scripts in the `tests/` folder. They drive the real `snapper.exe`
by injecting global hotkey + mouse input (no unit tests; these are end-to-end only).

Run from the repo root:

```
powershell -ExecutionPolicy Bypass -File tests\e2e_test.ps1     # record + pause/resume -> MP4
powershell -ExecutionPolicy Bypass -File tests\test_shot.ps1    # region screenshot -> PNG
powershell -ExecutionPolicy Bypass -File tests\test_sync.ps1    # A/V sync (full-screen flash+tone)
powershell -ExecutionPolicy Bypass -File tests\test_sync2.ps1   # A/V sync WITH pause/resume trim
powershell -ExecutionPolicy Bypass -File tests\visual_test.ps1  # screenshots of chooser/rec/pause bars
```

Notes:
- Build first (`cargo build`); the scripts launch `target\debug\snapper.exe`.
- They require `ffmpeg`/`ffprobe`/`ffplay` on PATH and a real desktop session (they move the
  real mouse and press real hotkeys, so don't run them while working at the machine).
- The button bars are driven via the `WM_SNAP_CLICK` (`0x8010`) hook sent to the
  `SnapperAction` / `SnapperCtl` windows (wParam = button id), not by clicking pixels.
  Action bar ids: 1=Record, 2=Screenshot, 3=Cancel. Ctl bar ids: 1=Pause/Resume toggle, 2=Stop.
- Output MP4/PNG land in `target\debug\output` (next to the debug exe the scripts launch);
  bar screenshots land in `%TEMP%`.
- Pass criteria: e2e produces a new MP4 (~7 s for the scripted run); `test_sync.ps1` shows the
  audio tone and video flash within ~50 ms; `test_sync2.ps1` shows the two event onsets separated
  by (clip gap − paused seconds) — i.e. the pause trim is correct.

## Module map (`src/`)

- `main.rs` — entry, single-instance mutex, tray setup, global hotkeys, the app state machine
  (idle → selecting → chooser → recording/paused), and the message pump that routes
  `WM_APP_*` messages to handlers. Owns `App` state and the `Tray`.
- `overlay.rs` — fullscreen selection overlay (dim + hairline + live size badge + fade-in) and the
  click-through recording border (accent line + glow). Posts `WM_APP_SELECTION` with the chosen rect.
- `ui.rs` — the Direct2D/DirectWrite capsule renderer: action bar, control bar (with live mm:ss
  timer), custom tray menu, and the tray icon (app PNG downscaled + state dots; MDL2 camera
  glyph fallback). All bar/menu icons are drawn as **geometry** (not font glyphs). Largest file.
- `theme.rs` — design tokens (colors, DIP metrics, fonts), DPI scale helpers, the one MDL2 glyph
  still used (tray camera).
- `capture.rs` — ffmpeg child-process lifecycle: spawn a **video-only** `ddagrab` region capture
  (`-an`) plus a **separate audio ffmpeg** fed by the WASAPI pipe, assign both to a Win32 job object
  (so children can't outlive the app), health check, and `finalize` (mux video + `audio.m4a` into one
  file with `-c copy`, then a clean `q` + exact re-encode trim of paused spans on the combined file).
- `audio.rs` — WASAPI loopback (system audio) + optional mic, streamed as raw PCM over a named pipe
  into the **audio ffmpeg** (a distinct process from the video grab — see Capture / pause model).
- `config.rs` — load/save `config.json` next to the exe (portable; `%APPDATA%\Snapper` fallback
  when the exe folder isn't writable, legacy `%APPDATA%` config auto-migrates). See README table.
- `util.rs` — small helpers (e.g. `wide`/`u16s` wide-string conversion, error helpers).

## Key constants & conventions

- Window classes: `SnapperOverlay`, `SnapperBorder` (overlay.rs); `SnapperAction`, `SnapperCtl`,
  `SnapperMenu` (ui.rs). The hidden main window class is registered in `main.rs`.
- App messages (custom `WM_APP_*`): `WM_APP_SELECTION=0x8001`, `WM_APP_ACTION=0x8002`,
  `WM_APP_CTL=0x8003`, `WM_APP_MENU=0x8004`. Test hook: `WM_SNAP_CLICK=0x8010`.
- Fade timers: `FADE_TIMER` is `0xF0` in ui.rs and `0xF1` in overlay.rs (per-window, must not clash).
- Hotkeys: record = `Win+Shift+R` (id `HOTKEY_REC=1`); pause = `Win+Alt+P` (id `HOTKEY_PAUSE=2`),
  falls back to `Ctrl+Alt+P`. Never use `Win+Shift+P` (Windows display-switch hotkey).
- Single instance: named mutex `Snapper.SingleInstance`.
- Output: `<exe dir>\output\<yyyy-MM-dd_HH-mm-ss>.mp4`/`.png` — a portable work folder next to
  `snapper.exe` (config-overridable). `capture.rs` does `create_dir_all` on demand.

## windows-rs 0.58 gotchas (cost hours — do not rediscover)

- `ID2D1DCRenderTarget::BindDC` **AV-crashes with a null sub-rect** on this `d2d1.dll`; always pass
  an explicit `&RECT{ left:0, top:0, right:w, bottom:h }`.
- `IDWriteFactory::CreateTextFormat` **rejects a NULL locale** — pass `w!("")`, not `None`/null.
- `ID2D1RenderTarget::CreateSolidColorBrush` is gated behind the `Foundation_Numerics` feature.
- A DC render target needs `.cast::<ID2D1RenderTarget>()` for draw calls, but `EndDraw` is called on
  the DC target itself as `EndDraw(None, None)`.
- `FillGeometry` takes 3 args — pass `None::<&ID2D1Brush>` for the opacity brush.
- `WM_MOUSELEAVE` lives in `Win32_UI_Controls`; it isn't pulled in by the current features, so it is
  defined locally as `0x02A3` in ui.rs.
- `GetDpiForMonitor`/`MDT_EFFECTIVE_DPI` are in `Win32_UI_HiDpi`; `MONITOR_*` in `Win32_Graphics_Gdi`.
- `D2D_RECT_F`/`D2D_POINT_2F` are in `Direct2D::Common`; `D2D1_ROUNDED_RECT`/`D2D1_ELLIPSE` in `Direct2D`.
- `tray-icon` 0.19 has **no `show_notification`** — use tooltips only.
- A 32-bit `BI_RGB` DIB from `CreateDIBSection` is BGRA in memory, i.e. the `u32` is
  `A<<24 | R<<16 | G<<8 | B` (byte 0 = B). When converting that back to RGBA8 (tray icon), R
  comes from the **low** byte — assuming `A|B|G|R` swaps R/B (red renders blue).

## UI rendering model

- Bars/menu: premultiplied-BGRA DIB + `ID2D1DCRenderTarget` (dpi `96*sc`) → `UpdateLayeredWindow`
  with `AC_SRC_ALPHA`. Button/row coordinates are **physical px** and must include the shadow margin
  `m = SHADOW_PAD*sc` (the window is larger than the capsule by that margin — forgetting `m` is what
  caused off-center buttons).
- Overlay/border: `make_surface(w,h,1.0)` (dpi 96) with physical-px coordinates; fonts and stroke
  widths are scaled by `sc` manually.
- Icons are geometry (record/stop/pause/play/camera/close/folder/power). MDL2 glyphs render as tofu
  through the DWrite `DrawText` path at bar sizes — only the tray camera glyph (E722) is kept.

## Capture / pause model

- The video grab ffmpeg records the whole session **video only** (`-an`); it is **never restarted**.
  A separate audio ffmpeg captures the WASAPI pipe to `audio.m4a`. Pause/resume just mark wall-clock
  spans; on stop, `q` closes both cleanly, `finalize` muxes video + `audio.m4a` (`-c copy`), then cuts
  the paused spans out with an exact re-encode trim (`trim`/`atrim`+`concat`) on the combined file,
  mapping wall clock → file timeline.
- **Gotcha — do NOT feed the live audio pipe into the grab ffmpeg as a second input.** With two live
  inputs, ffmpeg's muxer delays reading video and CFR-backfills several seconds of a frozen first
  frame (audio plays, image stuck ~4 s at the start). This was a regression (commit `5b5452b`); the
  video-only grab + separate audio + mux-at-finalize design fixes it.
- `Segment.start` is set to `t0` captured at **spawn** (not after the health check) so the video
  timeline matches when the border appears; the health check window is 250 ms.
- ffmpeg children (video grab + audio recorder) are assigned to a Win32 job object so they can never
  outlive the app.

## Style notes

- `anyhow::Result` with `.context(...)` for fallible setup; `windows` calls are `unsafe` blocks.
- Wide strings via the `wide`/`u16s` helpers in `util.rs`/`ui.rs`.
- Keep the design brief: deliberately NOT Windows-native — translucent near-black capsules, hairline
  strokes, icon-only controls, single accent (`#FF453A`), quiet fade motion. Tokens live in `theme.rs`.
- Git: branch `master`; commit messages are short lowercase sentences. Do not commit unless asked.
