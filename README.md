# Snapper

Lightweight Windows region screen recorder in Rust. Press `Win+Shift+R`, drag a rectangle,
get an MP4 — the QuickTime-style region recording that Windows 10 Snipping Tool lacks.

## Status

MVP working end-to-end: hotkey → dimmed fullscreen overlay → drag-select →
`ffmpeg ddagrab` capture of the region + **system audio (WASAPI loopback)** (+ optional mic mix) →
H.264/AAC MP4 → red border while recording, stop with the same hotkey, **pause/resume** with
`Win+Alt+P` (segments joined automatically). Verified by `e2e_test.ps1` (injects real
hotkey + mouse drag, checks output with ffprobe).

Not yet implemented (see plan.md): countdown, settings UI, installer.

## Usage

1. Make sure `ffmpeg.exe` (6.1+, 7.x recommended) is next to `snapper.exe` or on PATH.
2. Run `snapper.exe` — it lives in the system tray.
3. `Win+Shift+R` → screen dims → drag a region → recording starts (red border around region).
4. `Win+Alt+P` pauses/resumes (falls back to `Ctrl+Alt+P` if taken; `Win+Shift+P` is the Windows
   display-switch hotkey and is deliberately not used).
5. `Win+Shift+R` again (or tray → Stop) to finish. `Esc` / right-click cancels selection.
6. Files land in `%USERPROFILE%\Videos\Snapper\<yyyy-MM-dd_HH-mm-ss>.mp4`.

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
| `ffmpeg_path` | null | explicit ffmpeg.exe path |
| `output_idx_override` | null | force DXGI output index (monitor) |

## Build

```
cargo build --release     # target\release\snapper.exe (~0.6 MB, no runtime needed)
```

Test the full flow (drives the app with injected hotkey/mouse input):

```
powershell -ExecutionPolicy Bypass -File e2e_test.ps1
```

## How it works

- **Capture:** bundled ffmpeg uses the `ddagrab` lavfi source (DXGI Desktop Duplication) with
  `offset_x/offset_y/video_size` for the region and `hwdownload,format=bgra` to pull D3D11 frames
  to system memory. A fallback chain (region options → crop filter; `h264_mf` → `libx264`) adapts
  to older ffmpeg builds automatically.
- **Audio:** WASAPI capture (`IAudioClient`) on the default render endpoint with
  `AUDCLNT_STREAMFLAGS_LOOPBACK` (system sound) and optionally the default capture endpoint (mic).
  Each source is written to a raw PCM file during recording (silence is synthesized while the
  system is quiet); at finalize the files are concatenated, mixed via `amix` when a mic track
  exists, and muxed to AAC. If a source fails, recording degrades (mic-only failure → system
  audio only; audio failure → video-only).
- **UI:** pure Win32 via the `windows` crate — layered per-pixel-alpha windows for the dimmed
  selection overlay and the click-through recording border; `tray-icon` for the tray;
  `RegisterHotKey` for the global hotkey. Single instance via a named mutex.
- **Stop / pause:** `q` is written to ffmpeg's stdin, producing a clean, valid MP4; completion is
  detected by polling the child process from the message loop. Pause simply ends the current
  segment; resume starts a new one; on stop, segments are joined with the `concat` demuxer
  (`-c copy`) plus the audio mux, so the result is one continuous file. ffmpeg children are
  assigned to a Win32 job object so they can never outlive the app.
