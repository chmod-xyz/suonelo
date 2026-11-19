#!/usr/bin/env bash
# Build a thin macOS app for the native host architecture.
set -euo pipefail

fail() { printf 'suonelo: %s\n' "$*" >&2; exit 1; }
usage() { printf 'Usage: %s [--check | --app | --smoke-mounted APP]\n' "${0##*/}"; }
if [[ "${1:-}" == --smoke-mounted ]]; then
  [[ $# -eq 2 ]] || { usage >&2; exit 2; }
  [[ "$(uname -s)" == Darwin ]] || fail 'Run the smoke check on macOS.'
  bundle="$2"
  plist="$bundle/Contents/Info.plist"
  binary="$bundle/Contents/MacOS/$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist")"
  test -x "$binary"
  codesign --verify --deep --strict "$bundle"
  test "$(lipo -archs "$binary")" = x86_64
  libraries=$(otool -L "$binary")
  printf '%s\n' "$libraries"
  # Require system-provided dynamic libraries. Codec code is linked into the executable.
  awk 'NR > 1 && $1 !~ /^\/(usr\/lib|System\/Library)\// { print > "/dev/stderr"; bad=1 } END { exit bad }' <<< "$libraries"

  # A fresh profile uses an isolated database, webview session and Keychain account.
  profile="ci-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
  profile_dir="$HOME/Library/Application Support/com.suonelo.desktop/qa/$profile"
  test ! -e "$profile_dir"
  smoke="$(mktemp -d "$RUNNER_TEMP/dmg-smoke.XXXXXX")"
  cat > "$smoke/check-window.swift" <<'SWIFT'
  import CoreGraphics
  import Foundation
  import Darwin

  let pid = Int(CommandLine.arguments[1])!
  for _ in 0..<30 {
      if kill(pid_t(pid), 0) != 0 { exit(1) }
      let windows = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as? [[String: Any]] ?? []
      if windows.contains(where: { window in
          guard window[kCGWindowOwnerPID as String] as? Int == pid,
                window[kCGWindowLayer as String] as? Int == 0,
                let bounds = window[kCGWindowBounds as String] as? [String: Double] else {
              return false
          }
          return (bounds["Width"] ?? 0) >= 320 && (bounds["Height"] ?? 0) >= 520
      }) { exit(0) }
      Thread.sleep(forTimeInterval: 1)
  }
  exit(1)
SWIFT
  xcrun swiftc "$smoke/check-window.swift" -o "$smoke/check-window"
  (
    PATH=/usr/bin:/bin:/usr/sbin:/sbin /usr/bin/sandbox-exec \
      -p '(version 1) (allow default) (deny network* (remote ip "*:*"))' \
      /usr/bin/arch -x86_64 "$binary" "--qa-profile=$profile" > "$smoke/app.log" 2>&1 &
    app_pid=$!
    trap 'kill "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true' EXIT
    if ! "$smoke/check-window" "$app_pid" ||
       ! test -s "$profile_dir/data/suonelo.sqlite3" || ! kill -0 "$app_pid"; then
      cat "$smoke/app.log" >&2
      echo 'The mounted app did not open a window and initialize the isolated profile offline.' >&2
      exit 1
    fi
  )
  exit 0
fi

mode="${1:---check}"
[[ $# -le 1 ]] || { usage >&2; exit 2; }
case "$mode" in
  --help|-h) usage; exit 0 ;;
  --check|--app) ;;
  *) usage >&2; exit 2 ;;
esac

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
[[ "$(uname -s)" == Darwin ]] || fail 'Run in a native macOS shell.'
[[ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" != 1 ]] ||
  fail 'Open a native terminal session outside Rosetta.'
arch="$(uname -m)"
case "$arch" in
  arm64) target=aarch64-apple-darwin; node_arch=arm64 ;;
  x86_64) target=x86_64-apple-darwin; node_arch=x64 ;;
  *) fail "Unsupported macOS architecture: $arch" ;;
esac
for tool in node npm rustc cargo rustup xcode-select xcrun file lipo otool codesign shasum; do
  command -v "$tool" >/dev/null || fail "Missing tool: $tool"
done
xcode-select -p >/dev/null
sdk="$(xcrun --sdk macosx --show-sdk-path)"
[[ -d "$sdk" ]] || fail 'The selected Xcode tools have no macOS SDK.'
xcrun --sdk macosx --find clang >/dev/null

[[ "$(rustc -vV | awk '/^host: /{print $2}')" == "$target" ]] ||
  fail "Rust must run with host $target."
rustup target list --installed | grep -Fx "$target" >/dev/null ||
  fail "Install $target with rustup."
node - "$node_arch" <<'NODE'
const fs = require("node:fs");
const expectedArch = process.argv[2];
const app = require("./package.json");
const lock = require("./package-lock.json");
const cargoVersion = file => fs.readFileSync(file, "utf8")
  .match(/name = "suonelo"\r?\nversion = "([^"]+)"/)?.[1];
const tauri = require("./src-tauri/tauri.conf.json");
const versions = [lock.version, lock.packages[""].version, cargoVersion("src-tauri/Cargo.toml"),
  cargoVersion("src-tauri/Cargo.lock"), tauri.version];
const [major, minor] = process.versions.node.split(".").map(Number);
if (process.platform !== "darwin" || process.arch !== expectedArch || major < 20 || (major === 20 && minor < 9)) {
  throw Error("Use native " + expectedArch + " Node satisfying " + app.engines.node);
}
if (versions.some(version => version !== app.version)) {
  throw Error("Synchronize the application version across manifests and lockfiles.");
}
console.log("suonelo " + app.version + " using Node " + process.version);
NODE

# Generated output stays in this checkout. Use a disposable source copy for QA.
export CARGO_TARGET_DIR="$PWD/src-tauri/target"
export MACOSX_DEPLOYMENT_TARGET=13.0
export NEXT_PUBLIC_SUONELO_PLATFORM=darwin

if [[ "$mode" == --app ]]; then
  [[ -f src-tauri/tauri.macos.conf.json ]] || fail 'The macOS Tauri overlay is missing.'
  node - <<'NODE'
const fs = require("node:fs");
const config = require("./src-tauri/tauri.macos.conf.json");
const icons = config.bundle?.icon || [];
if (config.bundle?.active !== true || config.bundle?.macOS?.minimumSystemVersion !== "13.0" ||
    !icons.some(icon => icon.endsWith(".icns") && fs.existsSync("src-tauri/" + icon))) {
  throw Error("The macOS overlay must enable bundling, set macOS 13.0 and include a generated ICNS.");
}
NODE
fi

npm ci --include=optional
npm run typecheck
if [[ "$mode" == --app ]]; then
  # Tauri exports the frontend via beforeBuildCommand. Sign locally after bundling.
  npm run tauri -- build --target "$target" --bundles app --no-sign -- --locked
else
  npm run build -- --webpack
fi
cargo test --locked --target "$target" --manifest-path src-tauri/Cargo.toml
if [[ "$mode" == --check ]]; then exit 0; fi

bundle="$CARGO_TARGET_DIR/$target/release/bundle/macos/suonelo.app"
plist="$bundle/Contents/Info.plist"
[[ -f "$plist" ]] || fail "Missing bundle Info.plist at $plist"
plist_value() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist"; }
[[ "$(plist_value CFBundleIdentifier)" == com.suonelo.desktop ]] || fail 'Unexpected bundle identifier.'
version="$(node -p "require('./package.json').version")"
[[ "$(plist_value CFBundleShortVersionString)" == "$version" && "$(plist_value CFBundleVersion)" == "$version" ]] ||
  fail 'Bundle and source versions differ.'
[[ "$(plist_value LSMinimumSystemVersion)" == "$MACOSX_DEPLOYMENT_TARGET" ]] ||
  fail 'Bundle deployment target differs from the configured floor.'
[[ "$(plist_value CFBundleIconFile)" == icon.icns ]] &&
  cmp -s src-tauri/icons/icon.icns "$bundle/Contents/Resources/icon.icns" ||
  fail 'The bundled icon differs from the configured icon.'
binary="$bundle/Contents/MacOS/$(plist_value CFBundleExecutable)"
[[ -x "$binary" && "$(lipo -archs "$binary")" == "$arch" ]] ||
  fail "Expected a single $arch Mach-O executable."
file "$binary"
otool -L "$binary"
build_info="$(xcrun vtool -show-build "$binary")"
printf '%s\n' "$build_info"
[[ "$(awk '$1 == "minos" {print $2}' <<< "$build_info")" == "$MACOSX_DEPLOYMENT_TARGET" ]] ||
  fail 'Mach-O deployment target differs from the configured floor.'
codesign --force --sign - "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
shasum -a 256 "$binary"
printf 'Local QA bundle: %s\n' "$bundle"
