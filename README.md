# Snapper

Lightweight Windows region screen recorder in Rust. Press `Win+Shift+R`, drag a rectangle,
then choose **Record** (MP4) or **Screenshot** (PNG) — the QuickTime-style region capture that
Windows 10 Snipping Tool lacks.

## Status

MVP working end-to-end: hotkey → dimmed fullscreen overlay → drag-select → **Record / Screenshot**
chooser → `ffmpeg ddagrab` capture of the region. Recording adds **system audio (WASAPI loopback)**
(+ optional mic mix) → H.264/AAC MP4, red border while recording, **pause/resume** with `Win+Alt+P`
(segments joined automatically). Screenshot grabs one composited frame to PNG. Verified by
`e2e_test.ps1` (record) and `test_shot.ps1` (screenshot) — both inject real hotkey + mouse input.

Not yet implemented (see plan.md): countdown, settings UI, installer.

## Usage

1. Make sure `ffmpeg.exe` (6.1+, 7.x recommended) is next to `snapper.exe` or on PATH.
2. Run `snapper.exe` — it lives in the system tray (starts minimized, no window).
3. `Win+Shift+R` → screen dims → drag a region → a small **Record / Screenshot** bar appears.
4. **Record**: recording starts (red border) and a small floating **Pause/Resume · Stop** bar
   appears just outside the region. `Win+Alt+P` also pauses/resumes (falls back to `Ctrl+Alt+P` if
   taken; `Win+Shift+P` is the Windows display-switch hotkey and is deliberately not used).
   `Win+Shift+R` again (or the bar / tray → Stop) finishes.
5. **Screenshot**: writes a single PNG of the region.
6. `Esc` / right-click cancels selection; `Esc` (or `Win+Shift+R`) dismisses the chooser.
7. Files land in `%USERPROFILE%\Videos\Snapper\<yyyy-MM-dd_HH-mm-ss>.mp4` / `.png`.

## Configuration

`%APPDATA%\Snapper\config.json` (created on first run):

| key | default | meaning |
|---|---|---|
| `fps` | 60 | target frame rate |
| `bitrate_kbps` | 12000 | video bitrate for `h264_mf` |
| `encoder` | `auto` | `auto` (h264_mf → libx264 fallback), `h264_mf`, or `libx264` |
| `audio` | true | capture system audio (WASAPI loopback of default render device) |
| `mic` | false | also capture the default microphone and mix it with system audio |
| `audio_bitrate_kbps` | 192 | AAC bitrate |
| `output_dir` | `%USERPROFILE%\Videos\Snapper` | output folder |
| `screenshot_dir` | null | PNG folder (defaults to `output_dir`) |
| `ffmpeg_path` | null | explicit ffmpeg.exe path |
| `output_idx_override` | null | force DXGI output index (monitor) |

## Build

```
cargo build --release     # target\release\snapper.exe (~0.6 MB, no runtime needed)
```

Test the full flows (drive the app with injected hotkey/mouse input):

```
powershell -ExecutionPolicy Bypass -File e2e_test.ps1    # record + pause/resume
powershell -ExecutionPolicy Bypass -File test_shot.ps1   # region screenshot
```

## How it works

- **Capture:** bundled ffmpeg uses the `ddagrab` lavfi source (DXGI Desktop Duplication) with
  `offset_x/offset_y/video_size` for the region and `hwdownload,format=bgra` to pull D3D11 frames
  to system memory. A fallback chain (region options → crop filter; `h264_mf` → `libx264`) adapts
  to older ffmpeg builds automatically.
- **Screenshot:** same `ddagrab` path with `-frames:v 1` → PNG (true composited frame, parity with
  Snipping Tool). Triggered from the Record/Screenshot chooser after a region is selected.
- **Audio:** WASAPI capture (`IAudioClient`) on the default render endpoint with
  `AUDCLNT_STREAMFLAGS_LOOPBACK` (system sound) and optionally the default capture endpoint (mic).
  Each source is streamed as raw PCM over a named pipe **into the same ffmpeg process** that captures
  video, so ffmpeg timestamps audio and video on one clock (A/V stay in sync). With mic enabled the
  two are mixed via `amix`. Silence is synthesized while the system is quiet so sample-count
  timestamps keep tracking wall-clock time. If a source fails, recording degrades (mic-only failure →
  system audio only; audio failure → video-only).
- **UI:** pure Win32 via the `windows` crate — layered per-pixel-alpha windows for the dimmed
  selection overlay and the click-through recording border; a topmost floating control bar
  (Pause/Resume · Stop) anchored just outside the region; `tray-icon` for the tray;
  `RegisterHotKey` for the global hotkey. Single instance via a named mutex.
- **Stop / pause:** `q` is written to ffmpeg's stdin, producing a clean, valid MP4; completion is
  detected by polling the child process from the message loop. Pause simply ends the current
  segment; resume starts a new one; on stop, segments are joined with the `concat` demuxer
  (`-c copy`) — each segment is already muxed (video+audio), so the join is instant. ffmpeg children
  are assigned to a Win32 job object so they can never outlive the app.
