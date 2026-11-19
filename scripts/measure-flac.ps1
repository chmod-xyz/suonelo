[CmdletBinding()]
param(
    [ValidateRange(1, 300)][int]$ShortSeconds = 15,
    [ValidateRange(2, 1800)][int]$LongSeconds = 90,
    [ValidateRange(2, 12)][int]$ConcurrentJobs = 3
)

$ErrorActionPreference = 'Stop'
if ($LongSeconds -le $ShortSeconds) { throw 'LongSeconds must exceed ShortSeconds.' }
$repo = Split-Path -Parent $PSScriptRoot
$flac = (Get-Command flac.exe -CommandType Application).Source
$work = Join-Path ([System.IO.Path]::GetTempPath()) ('suonelo-flac-' + [guid]::NewGuid())
$testPrefix = 'media::codec::resource_test::'
$environment = 'SUONELO_FLAC_PCM', 'SUONELO_FLAC_SECONDS', 'SUONELO_FLAC_INPUT', 'SUONELO_FLAC_JOBS'
$prior = @{}
foreach ($key in $environment) { $prior[$key] = [Environment]::GetEnvironmentVariable($key) }

function Measure-Group([string]$program, [string[]]$invocations, [string]$directory) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $processes = @()
    $peak = 0L
    try {
        foreach ($arguments in $invocations) {
            $process = Start-Process -FilePath $program -ArgumentList $arguments -WorkingDirectory $directory -NoNewWindow -PassThru
            $processes += $process
            # Keep the process handle so Windows PowerShell can read the exit code.
            $null = $process.Handle
        }
        do {
            $active = $false
            $workingSet = 0L
            foreach ($process in $processes) {
                $process.Refresh()
                if (!$process.HasExited) {
                    $active = $true
                    try {
                        $workingSet += $process.WorkingSet64
                    } catch [InvalidOperationException] {
                        if (!$process.HasExited) { throw }
                    }
                }
            }
            $peak = [Math]::Max($peak, $workingSet)
            if ($active) { Start-Sleep -Milliseconds 10 }
        } while ($active)
        foreach ($process in $processes) {
            $process.WaitForExit()
            if ($process.ExitCode -ne 0) { throw "$program exited with code $($process.ExitCode)." }
        }
        [pscustomobject]@{ Seconds = $watch.Elapsed.TotalSeconds; PeakBytes = $peak }
    } finally {
        foreach ($process in $processes) {
            if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
            $process.Dispose()
        }
    }
}

function Read-StreamChecksum([string]$path) {
    $stream = [IO.File]::OpenRead($path)
    try {
        $stream.Position = 26
        $bytes = [byte[]]::new(16)
        if ($stream.Read($bytes, 0, 16) -ne 16) { throw "Missing STREAMINFO in $path" }
        [BitConverter]::ToString($bytes)
    } finally {
        $stream.Dispose()
    }
}

New-Item -ItemType Directory -Path $work | Out-Null
Push-Location $repo
try {
    $target = 'x86_64-pc-windows-msvc'
    $messages = cargo test --manifest-path src-tauri/Cargo.toml --locked --release --target $target --bin suonelo --no-run --message-format=json
    if ($LASTEXITCODE -ne 0) { throw 'The native test build failed.' }
    $testExe = $messages | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'suonelo' -and $_.profile.test } |
        Select-Object -ExpandProperty executable -Last 1
    if (!$testExe) { throw 'Cargo did not report the codec test executable.' }

    & $flac --version | Write-Host
    & $testExe --ignored --exact "${testPrefix}worker_bound_and_queued_cancellation" | Write-Host
    if ($LASTEXITCODE -ne 0) { throw 'The worker bound and cancellation test failed.' }

    $results = foreach ($seconds in $ShortSeconds, $LongSeconds) {
        $directory = (New-Item -ItemType Directory -Path (Join-Path $work $seconds)).FullName
        $env:SUONELO_FLAC_PCM = Join-Path $directory 'input.raw'
        $env:SUONELO_FLAC_SECONDS = $seconds
        $env:SUONELO_FLAC_INPUT = Join-Path $directory 'input.flac'

        & $testExe --ignored --exact "${testPrefix}generate_pcm" | Write-Host
        if ($LASTEXITCODE -ne 0) { throw 'PCM generation failed.' }
        & $flac -s -5 --threads=1 --force-raw-format --endian=little --sign=signed --channels=2 --bps=24 --sample-rate=192000 -o $env:SUONELO_FLAC_INPUT $env:SUONELO_FLAC_PCM
        if ($LASTEXITCODE -ne 0) { throw 'Reference FLAC creation failed.' }

        $referenceHash = (Get-FileHash -LiteralPath $env:SUONELO_FLAC_INPUT).Hash
        $checksum = Read-StreamChecksum $env:SUONELO_FLAC_INPUT
        $stream = [IO.File]::OpenWrite($env:SUONELO_FLAC_INPUT)
        try {
            $stream.Position = 26
            $stream.Write([byte[]]::new(16), 0, 16)
        } finally {
            $stream.Dispose()
        }

        foreach ($jobs in 1, $ConcurrentJobs) {
            $env:SUONELO_FLAC_JOBS = $jobs
            $native = Measure-Group $testExe @("--ignored --exact ${testPrefix}repair_and_verify") $directory
            $encode = @(0..($jobs - 1) | ForEach-Object { "-s -8 --threads=1 --verify -f -o xiph-$_.flac input.flac" })
            $verify = @(0..($jobs - 1) | ForEach-Object { "-s -t xiph-$_.flac" })
            $xiphEncode = Measure-Group $flac $encode $directory
            $xiphVerify = Measure-Group $flac $verify $directory
            for ($job = 0; $job -lt $jobs; $job++) {
                $nativeFile = Join-Path $directory "native-$job.flac"
                & $flac -s -t $nativeFile
                if ($LASTEXITCODE -ne 0) { throw 'The independent decoder rejected the native repair.' }
                if ((Get-FileHash -LiteralPath $nativeFile).Hash -ne $referenceHash) {
                    throw 'Native repair did not restore the original FLAC bytes.'
                }
                if ((Read-StreamChecksum (Join-Path $directory "xiph-$job.flac")) -ne $checksum) {
                    throw 'The Xiph baseline did not preserve the original PCM checksum.'
                }
            }
            [pscustomobject]@{
                AudioSeconds = $seconds
                Jobs = $jobs
                NativeSeconds = [Math]::Round($native.Seconds, 3)
                NativeMiB = [Math]::Round($native.PeakBytes / 1MB, 2)
                XiphSeconds = [Math]::Round($xiphEncode.Seconds + $xiphVerify.Seconds, 3)
                XiphMiB = [Math]::Round([Math]::Max($xiphEncode.PeakBytes, $xiphVerify.PeakBytes) / 1MB, 2)
            }
        }
    }
    $results
} finally {
    Pop-Location
    foreach ($key in $environment) { [Environment]::SetEnvironmentVariable($key, $prior[$key]) }
    $resolved = (Resolve-Path -LiteralPath $work).Path
    if ($resolved -ne $work) { throw "Unexpected benchmark workspace: $resolved" }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
