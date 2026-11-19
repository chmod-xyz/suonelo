param([string]$OutputRoot = $PSScriptRoot)

$ErrorActionPreference = 'Stop'
$appRoot = $PSScriptRoot
$manifest = Join-Path $appRoot 'src-tauri/Cargo.toml'
$target = 'x86_64-pc-windows-msvc'
$oldTarget = $env:CARGO_TARGET_DIR
$oldTelemetry = $env:NEXT_TELEMETRY_DISABLED
$oldPlatform = $env:NEXT_PUBLIC_SUONELO_PLATFORM

Push-Location -LiteralPath $appRoot
try {
    $env:CARGO_TARGET_DIR = Join-Path $appRoot 'src-tauri/target'
    $env:NEXT_TELEMETRY_DISABLED = '1'
    $env:NEXT_PUBLIC_SUONELO_PLATFORM = 'win32'

    npm ci
    if ($LASTEXITCODE -ne 0) { throw 'npm dependency installation failed.' }
    npm run build -- --webpack
    if ($LASTEXITCODE -ne 0) { throw 'The frontend export failed.' }
    cargo build --release --locked --target $target --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'The Windows build failed.' }

    & (Join-Path $appRoot 'package-portable.ps1') -OutputRoot $OutputRoot
} finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:NEXT_TELEMETRY_DISABLED = $oldTelemetry
    $env:NEXT_PUBLIC_SUONELO_PLATFORM = $oldPlatform
    Pop-Location
}
