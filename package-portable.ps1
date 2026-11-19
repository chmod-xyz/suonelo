param([string]$OutputRoot = $PSScriptRoot)

$ErrorActionPreference = 'Stop'
$appRoot = $PSScriptRoot
$exe = Join-Path $appRoot 'src-tauri/target/x86_64-pc-windows-msvc/release/suonelo.exe'
$version = (Get-Content -Raw -LiteralPath (Join-Path $appRoot 'src-tauri/tauri.conf.json') | ConvertFrom-Json).version
$taxonomy = Get-Content -Raw -LiteralPath (Join-Path $appRoot 'src-tauri/src/search/genres.json') | ConvertFrom-Json
if (-not $taxonomy.storefrontCoverageVerified -or $taxonomy.storefrontNodes -ne $taxonomy.genres.Count) {
    throw 'The bundled Qobuz genre taxonomy has not passed coverage verification.'
}
if ((Get-Item -LiteralPath $exe).VersionInfo.ProductVersion -ne $version) {
    throw 'The executable version does not match the portable package version.'
}
$stage = Join-Path $OutputRoot "suonelo-$version-windows-x64"
$zip = Join-Path $OutputRoot "suonelo-$version-windows-x64.zip"
if ((Test-Path -LiteralPath $zip) -or (Test-Path -LiteralPath "$zip.sha256")) {
    throw 'A package already exists at this location. Use a new output root for the candidate.'
}
New-Item -ItemType Directory -Force -Path $stage | Out-Null

$files = [ordered]@{
    'suonelo.exe' = $exe
    'REPOSITORY-MIT-LICENSE.txt' = (Join-Path $appRoot 'LICENSE')
    'APPLICATION-GPL-3.0.txt' = (Join-Path $appRoot 'licenses/APPLICATION-GPL-3.0.txt')
    'DECIBRI-APACHE-2.0.txt' = (Join-Path $appRoot 'licenses/DECIBRI-APACHE-2.0.txt')
    'LUCIDE-LICENSE.txt' = (Join-Path $appRoot 'licenses/LUCIDE-LICENSE.txt')
    'SPACE-GROTESK-OFL.txt' = (Join-Path $appRoot 'licenses/SPACE-GROTESK-OFL.txt')
    'ASSET-NOTICES.md' = (Join-Path $appRoot 'docs/ASSET-NOTICES.windows.md')
    'CHANGELOG.md' = (Join-Path $appRoot 'docs/CHANGELOG.md')
    'README.md' = (Join-Path $appRoot 'README.md')
}
$frontendNotices = foreach ($dependency in (Get-Content -Raw -LiteralPath (Join-Path $appRoot 'package.json') | ConvertFrom-Json).dependencies.PSObject.Properties.Name) {
    $module = Join-Path $appRoot "node_modules/$dependency"
    $metadata = Get-Content -Raw -LiteralPath (Join-Path $module 'package.json') | ConvertFrom-Json
    "$dependency $($metadata.version)"
    $notices = Get-ChildItem -LiteralPath $module -File | Where-Object { $_.Name -match '^(license|licence|copying|notice)(\.|$)' }
    if ($notices.Count -eq 0) { "Declared license: $($metadata.license)" }
    foreach ($notice in $notices) { Get-Content -Raw -LiteralPath $notice.FullName }
    ""
}
$frontendPath = Join-Path $stage 'FRONTEND-LICENSES.txt'
$frontendNotices | Set-Content -Encoding utf8 -LiteralPath $frontendPath
$archiveFiles = foreach ($name in $files.Keys) {
    $path = Join-Path $stage $name
    Copy-Item -LiteralPath $files[$name] -Destination $path -Force
    $path
}
Compress-Archive -LiteralPath @($archiveFiles; $frontendPath) -DestinationPath $zip
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    $expected = @($files.Keys; 'FRONTEND-LICENSES.txt')
    if (Compare-Object ($expected | Sort-Object) ($archive.Entries.FullName | Sort-Object) -CaseSensitive) {
        throw 'The portable ZIP has unexpected or missing entries.'
    }
    foreach ($entry in $archive.Entries) {
        $stream = $entry.Open()
        try {
            $archivedHash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash
            $expectedHash = (Get-FileHash -LiteralPath (Join-Path $stage $entry.Name) -Algorithm SHA256).Hash
            if ($archivedHash -ne $expectedHash) { throw "ZIP verification failed for $($entry.Name)." }
        } finally {
            $stream.Dispose()
        }
    }
} finally {
    $archive.Dispose()
}
$sha256 = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
"$sha256  $(Split-Path -Leaf $zip)" | Set-Content -LiteralPath "$zip.sha256" -Encoding ascii
Write-Output "Created $zip"
