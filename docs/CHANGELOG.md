# suonelo changelog

## Unreleased

## [1.0.10] - 2026-11-20

### Changed

- [win/mac] Show one row per album for artist discographies and playlists. Row actions affect that album, and a playlist still exports one M3U in the original track order, including repeats.
- [win/mac] Show outlined Remove buttons and icon-only file actions with tooltips. Center the file controls vertically and give them 12 px of spacing.

### Fixed

- [win/mac] Enable Open folder as soon as the first track is saved so each downloading release opens its own folder directly.
- [win/mac] Let release-level Retry recover failed files of any type, including timed-out lyrics, while keeping successfully saved files and the release's original file-type settings.

**Validation:** Windows x64 release build and 136 Rust tests passed. Native WebView2 smoke tests and mocked browser regressions for the release and file rows passed.

## [1.0.9] - 2026-11-19

### Changed

- [win/mac] Repair missing FLAC audio checksums automatically and verify the tagged file before saving it.
- [win/mac] Fixed spacing and styling issues
- [win/mac] Added tooltips to release row buttons

### Removed

- [win/mac] Removed optional FLAC re-encoding, external FLAC tool setup, and the FLAC settings tab. Checksum repair retains the original encoded audio frames.

**Validation:** Windows x64 release-mode Rust tests passed, including FLAC checksum repair and staged-file verification. Native smoke tests passed for portable profile upgrades and compatibility IPC.

## [1.0.8] - 2026-11-18

### Changed

- Combined the Windows and macOS apps in one React and Rust codebase while preserving each platform's native webview and credential storage.
- Packaged Windows x64 as a portable ZIP with the app's profile stored beside the executable.
- Targeted Intel Macs with an x86_64 DMG that requires Rosetta on supported Apple silicon Macs.

**Validation:** On Windows x64, 125 Rust tests passed and 10 were ignored. TypeScript checks, the frontend export, and mocked UI checks passed.
