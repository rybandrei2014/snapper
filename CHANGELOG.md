# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Release notes for GitHub Releases are taken from the matching `## [vX.Y.Z]` section below (see `.github/workflows/release.yml`).

## [Unreleased]

## [v0.2.0] - 2026-10-25

### Added

- Custom app icon: the tray icon and the embedded `.exe` icon are now generated from a 256px master (`assets\icon.png`), replacing the MDL2 glyph fallback.

### Fixed

- Recording no longer starts with a ~4-second frozen first frame while audio plays. The screen grab now records video only and audio is captured by a separate ffmpeg, muxed at finalize; feeding the live audio pipe into the grab ffmpeg had made its muxer delay the video and back-fill a frozen first frame.

## [v0.1.0] - 2026-10-18

### Added

- Lightweight Windows region screen recorder: tray-only, `Win+Shift+R` → dimmed overlay → drag a rectangle → Record (MP4) or Screenshot (PNG).
- Region capture via `ffmpeg ddagrab` + system audio (WASAPI loopback, optional mic), red recording border, Pause/Stop capsule bar, pause/resume with paused spans trimmed out on stop.
- Portable build (static CRT — no VC++ redistributable, installer, or bundled runtime); GitHub Actions release bundles a pinned ffmpeg.

[Unreleased]: https://github.com/rybandrei2014/snapper/compare/v0.2.0...HEAD
[v0.2.0]: https://github.com/rybandrei2014/snapper/compare/v0.1.0...v0.2.0
[v0.1.0]: https://github.com/rybandrei2014/snapper/releases/tag/v0.1.0
