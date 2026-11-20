# suonelo 1.0.10

suonelo is a desktop app for browsing Qobuz and saving releases, tracks, and booklets. Connect a Qobuz account to use the catalog. Full-track downloads require an eligible account.

## Run the app

- **Windows x64** · Extract the portable ZIP into a writable folder and run `suonelo.exe`. Install Microsoft Edge WebView2 Evergreen Runtime if needed. The portable profile lives in `data` beside the executable. Close both copies of the app before moving the whole profile. Windows protects saved credentials, so moving the profile to a different account or machine can require signing in again.
- **macOS** · Download `suonelo-1.0.10-macos-x86_64.dmg` from the matching GitHub Release. It runs natively on Intel Macs and requires Rosetta on supported Apple silicon Macs. The DMG is unsigned and the app is ad-hoc signed. Mount the DMG and move `suonelo.app` to Applications. The app declares macOS 13.0 as its minimum version. It uses the system WKWebView, stores its profile in `~/Library/Application Support/com.suonelo.desktop`, and saves credentials in Keychain.

FLAC checksum repair is built in. The Rust decoder `decibri-decode` streams PCM samples to compute a missing audio MD5. suonelo writes the checksum into the FLAC STREAMINFO header without re-encoding the audio frames, then verifies the tagged file before saving it. Both packages include the decoder's Apache License 2.0 and work without an installed FLAC tool.

## Build from source

Use Node.js 20.9 or later and Rust 1.99.1. CI uses Node.js 24. Build from this directory with the native SDK for your platform.

### Windows x64

Install the MSVC C++ build tools and Windows SDK. Install the Rust target `x86_64-pc-windows-msvc`.

```powershell
pwsh -File build-portable.ps1 -OutputRoot src-tauri/target/my-candidate
cargo test --release --locked --target x86_64-pc-windows-msvc --manifest-path src-tauri/Cargo.toml
```

Choose a fresh output directory for each package. The build installs npm dependencies from the lockfile, exports the frontend, compiles the executable and creates `suonelo-1.0.10-windows-x64.zip` with a SHA-256 sidecar. To package an existing x64 release build, run `package-portable.ps1` with the same output option. Windows CI tests x64.

### macOS

Install the Xcode Command Line Tools and use a native Terminal session with matching Rust and Node. Install the Rust target `aarch64-apple-darwin` on Apple Silicon or `x86_64-apple-darwin` on Intel.

```bash
bash scripts/build-macos.sh --check
bash scripts/build-macos.sh --app
```

The check lane installs locked npm dependencies, typechecks and exports the frontend, and runs locked Rust tests. The app lane builds and verifies an ad-hoc signed `suonelo.app` under `src-tauri/target/<rust-target>/release/bundle/macos/`. The isolated build wrapper `scripts/build-macos-stable.py` remaps source paths. Run `python3 scripts/scan-macos-release-privacy.py path/to/suonelo.app` on the resulting bundle.

macOS CI builds the `x86_64-apple-darwin` target on `macos-26-intel`. It checks the bundle and checksum, and launches the mounted DMG with a fresh QA profile on Intel. Pushing the matching `v1.0.10` tag publishes the verified Windows x64 ZIP, Intel DMG and a combined `SHA256SUMS.txt` to GitHub Releases. To verify your download, put all three files in the same directory and run `shasum -a 256 -c SHA256SUMS.txt`.

### Frontend QA

Run `npm ci`, `npm run typecheck` and `npm run build -- --webpack`. To export the UI for the other platform, set `NEXT_PUBLIC_SUONELO_PLATFORM` to `win32` or `darwin` before running the build.

## Changelogs and notices

The root `LICENSE` contains the repository's MIT license. The suonelo application is licensed under GPL-3.0-or-later and credits the original [HongYue1/qobuz-cli](https://github.com/HongYue1/qobuz-cli) application. The GPL text is in `licenses/APPLICATION-GPL-3.0.txt`. Both license texts are included in the Windows portable ZIP and macOS DMG.

See the [changelog](docs/CHANGELOG.md) and [asset notices](ASSET-NOTICES.md).
