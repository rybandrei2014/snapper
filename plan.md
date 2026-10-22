# Snapper — Windows Region Screen Recorder: Viability & Implementation Plan

## 1. Problem Statement

The Windows Snipping Tool captures still images well but (on Windows 10 and older Windows 11 builds) lacks
**region screen recording**: select an arbitrary rectangle on screen and record it as a video file,
the way QuickTime does on macOS. Build a small, fast, tray-based Windows app that does exactly that.

> Note: Microsoft started shipping screen recording in Snipping Tool for Windows 11 (2024 updates).
> If you are on Windows 10, or want features Snipping Tool still lacks (region snapping, pause/resume,
> bitrate control, GIF export, auto-naming, hotkey workflow), the project remains justified.

## 2. Viability Analysis

**Verdict: fully viable.** Every capability required is a documented, supported OS primitive:

| Capability | OS / API support |
|---|---|
| Grab composited screen pixels of a monitor | **DXGI Desktop Duplication** (Win 8+) or **Windows.Graphics.Capture** (Win 10 1809+) — both capture the final composited frame, so any region can be cropped out |
| Crop to user-selected rectangle | GPU copy/viewport or FFmpeg `crop` filter |
| Include mouse cursor | WGC `IsCursorCaptureEnabled`; DXGI duplication exposes pointer data; FFmpeg `ddagrab` has `draw_cursor` |
| H.264/HEVC encoding (HW accelerated) | **Media Foundation** (built into Windows, exposed by FFmpeg as `h264_mf`/`hevc_mf`, or NVENC/AMF/QSV) |
| System (loopback) audio | **WASAPI loopback** capture (NAudio in .NET, or custom) |
| Microphone audio | WASAPI capture, mix with loopback before encoding |
| Region-selection overlay UI | Transparent, topmost, fullscreen window (WPF/Win32 layered window) — proven pattern (ShareX, Greenshot) |
| Global hotkey | Win32 `RegisterHotKey` |

Known limitations (shared by ALL capture tools, including Snipping Tool and OBS):

- DRM-protected content (Widevine: Netflix, etc.) appears **black** — OS-level, unavoidable.
- Secure desktop (UAC prompts, lock screen) cannot be captured.
- Requires Windows 10 1809+ (realistically: Win 10 1903+ / Win 11).

Precedents proving feasibility: **ShareX** (C#/WPF + FFmpeg — does exactly region video today),
OBS Studio, Greenshot, Captura. This is a solved problem class; the work is product polish, not R&D.

## 3. Technology Stack — Options Considered

| Option | Stack | Pros | Cons | Verdict |
|---|---|---|---|---|
| A | **C++/WinRT + WGC + D3D11 + Media Foundation** | Max performance, zero deps, closest to how Microsoft builds it | Slowest development, most interop code, hardest to iterate | Overkill for v1 |
| B | **C# / .NET 8 + WPF + FFmpeg sidecar + NAudio** | Fast UI dev; FFmpeg's `ddagrab` (DXGI duplication) does capture+crop+encode in one proven process; NAudio handles WASAPI loopback; ShareX precedent | Ships ffmpeg.exe (~10–20 MB); ~100 MB self-contained binary | Runner-up |
| C | Rust + `windows-rs` (Win32/WGC) + FFmpeg sidecar | ~0.6 MB native binary, no runtime, memory safety; `windows-rs` gives first-class Win32 bindings | Overlay UI hand-rolled in Win32 (done); slower UI iteration | ✅ **Chosen** |
| D | Electron/Tauri + native capture plugin | Fancy web UI | Heavy, no UI advantage for an overlay tool | No |
| E | Python + mss + OpenCV + FFmpeg | Prototype in hours | GIL + GDI capture = CPU-bound, no polished UX | Spike/throwaway only |

### Chosen stack (Option C — Rust, decided 2026-10-22)

- **Language/runtime:** Rust (stable MSVC), `windows` crate 0.58 for Win32, `tray-icon`/`muda` for tray,
  `anyhow`/`serde`/`chrono`. Release binary ~0.6 MB, self-contained, x64 + ARM64-capable.
- **UI:** raw Win32 layered windows (per-pixel alpha via `UpdateLayeredWindow`) for the dimmed
  selection overlay and the click-through recording border; hidden message window + `RegisterHotKey`.
- **Capture + video encode:** bundled **FFmpeg** sidecar: `ddagrab` (DXGI Desktop Duplication) with
  `offset_x/offset_y/video_size` region options + `hwdownload,format=bgra` → `h264_mf` (HW encoder),
  with an automatic fallback chain for older ffmpeg versions and encoders.
- **Audio (M2):** WASAPI loopback + mic via `windows` crate → raw PCM file per segment → `amix` + AAC
  muxed at finalize (named-pipe input stalled ffmpeg on this build; file-based muxing is simpler/robust).
- **Packaging:** Inno Setup or WiX installer; code signing; optional MS Store.

Why the FFmpeg sidecar instead of hand-rolled WGC+MF? It removes the two riskiest chunks of the project
(frame acquisition and encoder interop) on day one, at the cost of one bundled executable. CPU cost of
`ddagrab + hwdownload + h264_mf` for a 1080p60 region is a few percent on modern hardware. If profiling
ever shows a bottleneck, the capture path can be swapped to native WGC (`windows-rs` supports it) without
touching the rest of the app — see §7 v2.

## 4. Architecture

```
┌────────────────────────────────────────────────────────────┐
│ snapper.exe (Rust, ~0.6 MB, tray-resident)                 │
│                                                            │
│  RegisterHotKey(Win+Shift+R) ──► state machine (main.rs)   │
│  (hidden message window)         │                         │
│                                  ├─► overlay.rs            │
│                                  │    dimmed fullscreen    │
│                                  │    layered window,      │
│                                  │    drag-select rect,    │
│                                  │    red recording border │
│                                  │    (click-through)      │
│                                  ├─► capture.rs            │
│                                  │    ffmpeg sidecar:      │
│                                  │    ddagrab → hwdownload │
│                                  │    → h264_mf, stop via  │
│                                  │    'q' on stdin         │
│                                  ├─► audio.rs              │
│                                  │    WASAPI loopback+mic  │
│                                  │    → raw PCM files →    │
│                                  │    amix+mux at finalize │
│                                  └─► output naming         │
│                                       Videos\Snapper\*.mp4 │
│  tray-icon ◄── start / stop / open folder / quit           │
└────────────────────────────────────────────────────────────┘
```

### Capture pipeline (implemented)

```
ffmpeg -f lavfi -i "ddagrab=output_idx=0:framerate=60:draw_mouse=1:offset_x=X:offset_y=Y:video_size=WxH" \
       -vf "hwdownload,format=bgra" \
       -c:v h264_mf -b:v 12000K -pix_fmt nv12 -an -f mp4 -y out.mp4
```

- `offset_x/offset_y/video_size` crop **during capture** (monitor-relative coords) — cheaper than a
  crop filter; `hwdownload` is required because recent ffmpeg builds output D3D11 hardware frames.
- A variant chain retries with `-vf crop=` and `libx264` automatically on older ffmpeg builds.
- **Pause/resume (M2, implemented):** record numbered segments (`name.seg000.mp4`…), stop each with `q`,
  join video with the `concat` demuxer (`-c copy`) and audio by concatenating raw PCM, then mux once.
  ffmpeg children live in a Win32 job object (`KILL_ON_JOB_CLOSE`) so orphans can't hold the DXGI session.
- **Stop:** send `q` to ffmpeg stdin → clean moov atom, valid MP4.
- **Health check caveat:** never gate startup on output file size — static screens encode at ~0.1 kbps
  and the mp4 muxer may not flush 1 KB for several seconds; only early process exit marks a variant dead.

### State machine

`Idle → Selecting → Choosing(Record/Screenshot) → Recording ⇄ Paused → Finalizing → Idle`

## 5. Milestones & Tasks

### M0 — Feasibility spike ✅ DONE (2026-10-22)
- `ddagrab` verified on this machine: needs `hwdownload,format=bgra` (D3D11 frames) and
  `draw_mouse` (not `draw_cursor`) on the installed ffmpeg (gyan 2024-04 build).
- 1080p-class region → clean MP4 via `h264_mf`, CPU negligible.

### M1 — MVP ✅ DONE (2026-10-22, Rust)
- [x] Cargo project: `main.rs` (state machine, tray, hotkey), `overlay.rs`, `capture.rs`, `util.rs`, `config.rs`
- [x] Global hotkey `Win+Shift+R` → fullscreen dimmed layered-window overlay, drag-to-select, Esc/RMB cancel
- [x] ffmpeg spawn with region options; red click-through border while recording; stop via same hotkey
- [x] MP4 in `Videos\Snapper\yyyy-MM-dd_HH-mm-ss.mp4`; tray menu (start/stop/open folder/quit); single instance
- [x] `e2e_test.ps1`: injects hotkey + mouse drag, verifies output with ffprobe — **passing**
- Not yet: live size label during drag, resize handles, floating control bar (tray/hotkey stop instead)

### M2 — Audio & recording features (core done 2026-10-22)
- [x] System (loopback) audio: WASAPI → raw PCM **named pipe into the same ffmpeg** → AAC, so ffmpeg
      timestamps A/V on one clock (48 kHz stereo, mix format auto-detected; `audio: false` switch;
      silence padding keeps the stream continuous while the system is quiet) ✅ verified: 440 Hz tone
      → mean −23 dB in output
- [x] Mic input mixed with loopback (second WASAPI capture → second pipe → `amix` inside ffmpeg;
      `mic: true` switch; degrades gracefully when no mic present) ✅ amix graph verified; real-mic
      path untested (dev machine has no capture device)
- [x] **A/V sync fix:** audio now shares ffmpeg's clock (was: separate raw-file capture started
      ~1.2 s after video → sound led picture by ~1.2 s). `test_sync.ps1` measures flash-vs-tone
      offset in a recording: now ~50–150 ms (was ~1200 ms); residual is near the perceptual
      threshold and partly the test player's own output latency.
- [x] Pause/resume with segment concat: `Win+Alt+P` (fallback `Ctrl+Alt+P`; `Win+Shift+P` is the
      Windows display-switch hotkey — do not use), tray Pause/Resume/Stop with enable states ✅
      verified: 3 s + pause 2 s + 1.5 s → single 4.5 s MP4 with audio, twice in a row
- [x] Region screenshot (Snipping-Tool style): same drag-select, then a Record/Screenshot chooser;
      screenshot = `ddagrab -frames:v 1` → PNG in `screenshot_dir` (defaults to `output_dir`) ✅
      verified: 801×501 region → 801×501 PNG (`test_shot.ps1`)
- [x] Floating control bar during recording (Pause/Resume · Stop) anchored just outside the
      region, in addition to the hotkey and tray menu ✅ verified via `e2e_test.ps1` button clicks
- [ ] Per-source volume knobs in settings
- [ ] Countdown timer, re-select-last-region (`Win+Shift+G`)
- [ ] Quality presets (1080p/4K, 30/60 fps, bitrate), output format MP4/MKV
- [ ] Per-monitor DPI correctness across mixed-DPI multi-monitor setups
- **Acceptance:** A/V in sync after 10-min recording; pause/resume produces one valid file. ✅ latter done

### M3 — v1.0 release polish (2–3 weeks)
- [ ] Settings window (hotkeys, output folder, quality, audio devices, run-on-startup).
- [ ] Crash handling + ffmpeg stderr logging; "recording interrupted" recovery of segments.
- [ ] Installer (Inno Setup), **code signing**, auto-update check.
- [ ] Performance pass: idle RAM < 100 MB, recording CPU budget met.
- **Acceptance:** install on a clean Win 10/11 VM, no SmartScreen scare, 8-hour soak stable.

### M4 — v1.x differentiators (ongoing)
- [ ] GIF export (region → GIF/APNG), trim-after-record quick edit.
- [ ] Window snapping during selection (edge snap to window bounds via `EnumWindows` geometry).
- [ ] Click highlighting / keystroke overlay for tutorials.
- [ ] Native WGC capture pipeline (see §7) if ffmpeg path shows CPU/latency limits.
- [ ] Clipboard copy of recording, simple upload (0x0.st/S3) with explicit user opt-in.

**Total effort (solo):** MVP in ~2 weeks, v1.0 in ~6–8 weeks part-time.

## 6. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| `ddagrab` needs FFmpeg ≥ 6.1 (cursor options ≥ 7.0) | Build breakage | Bundle pinned FFmpeg 7.x build with the app |
| Mixed-DPI multi-monitor coordinate math | Wrong crop region | Use virtual-screen coords + `PerMonitorV2` DPI awareness; test matrix |
| A/V drift on long recordings | Out-of-sync audio | `-fflags +genpts`, wallclock timestamps; concat segments ≤ 10 min |
| Audio device change mid-recording | Lost audio | Restart loopback capture on device callback, keep timestamps |
| DRM content records black | User confusion | Document; show one-time notice |
| SmartScreen blocks unsigned exe | Adoption friction | OV code-signing cert (~$100–400/yr) before public release |
| WPF overlay quirks (topmost fights, click-through) | UX bugs | Proven pattern: `Topmost` + `ShowInTaskbar=false` + WS_EX_TOOLWINDOW; ShareX precedent |
| 4K fullscreen recording CPU | Jank | Region crop before encode; default 60 fps cap; HW encoder default |

## 7. v2 Native Pipeline (only if needed)

Replace ffmpeg's capture with **Windows.Graphics.Capture**:
`Direct3D11CaptureFramePool` on the monitor → GPU `CopySubresourceRegion` of the crop rect →
staging texture → `Map` → rawvideo pipe into ffmpeg (encoder unchanged), or full zero-copy
Media Foundation `IMFTransform` H.264 encoder fed by the D3D11 texture.
Benefits: lower CPU, guaranteed cursor rendering, HDR path, window-scoped capture (capture a single
window even when occluded). Cost: significant interop code (C# via Vortice/ComWrappers or C++).
Decision gate: only if M3 profiling shows the ffmpeg path inadequate.

## 8. Repository Layout

```
snapper/
  src/
    main.rs       # state machine, tray menu, hotkey, single instance, message loop
    overlay.rs    # selection overlay + recording border (layered windows)
    capture.rs    # ffmpeg sidecar: variant fallback chain, stdin stop, segment polling, finalize mux
    util.rs       # monitors (EnumDisplayMonitors), rect math, ffmpeg lookup
    config.rs     # %APPDATA%\Snapper\config.json
  e2e_test.ps1    # injected-input end-to-end test (hotkey + drag + record + ffprobe check)
  test_shot.ps1   # injected-input screenshot test (hotkey + drag + chooser + ffprobe check)
  test_sync.ps1   # A/V sync test (flash+beep clip, measures audio-vs-video offset)
  plan.md  design.md  README.md
  target/release/snapper.exe   # ~0.6 MB; ship ffmpeg.exe beside it
```

Licensing: app MIT/proprietary (your choice); ship LGPL-licensed FFmpeg build (no GPL filters in MVP);
`windows`, `tray-icon`, `muda`, `anyhow`, `serde`, `chrono` all MIT/Apache-2.0.

## 9. MVP Acceptance Criteria (definition of done)

1. `Win+Shift+R` anywhere → drag a region → recording starts within 500 ms.
2. 1080p60 region recorded 10 min: file plays in Edge/VLC/PotPlayer, A/V in sync, CPU < 10% (HW encode).
3. Cursor visible in capture; system audio captured; Esc/⏹ stop produces valid MP4 every time.
4. Works on Windows 10 1903+ and Windows 11, single and mixed-DPI dual monitors.
5. Idle footprint: tray only, < 100 MB RAM, no visible window.
