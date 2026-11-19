# suonelo changelog

## Unreleased

## [1.0.9] - 2026-11-19

### Changed

- [win/mac] Repair missing FLAC audio checksums automatically and verify the tagged file before saving it.

### Removed

- [win/mac] Removed optional FLAC re-encoding, external FLAC tool setup, and the FLAC settings tab. Checksum repair retains the original encoded audio frames.

**Validation:** Windows x64 release-mode Rust tests passed, including FLAC checksum repair and staged-file verification. Native smoke tests passed for portable profile upgrades and compatibility IPC.

## [1.0.8] - 2026-11-18

### Changed

- Combined the Windows and macOS apps in one React and Rust codebase while preserving each platform's native webview and credential storage.
- Packaged Windows x64 as a portable ZIP with the app's profile stored beside the executable.
- Targeted Intel Macs with an x86_64 DMG that requires Rosetta on supported Apple silicon Macs.

**Validation:** On Windows x64, 125 Rust tests passed and 10 were ignored. TypeScript checks, the frontend export, and mocked UI checks passed.
