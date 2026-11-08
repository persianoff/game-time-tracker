# Build release and zip both exes, README, the checklist and the viewer into dist\gametime-<version>.zip.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $m = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $m) { throw 'version not found in Cargo.toml' }
    $version = $m.Matches[0].Groups[1].Value

    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }

    $stage = Join-Path $root "dist\gametime-$version"
    $zip = Join-Path $root "dist\gametime-$version.zip"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    if (Test-Path $zip) { Remove-Item -Force $zip }
    New-Item -ItemType Directory -Force (Join-Path $stage 'viewer') | Out-Null

    Copy-Item target\release\gametime.exe, target\release\gametime-tracker.exe, README.md $stage
    Copy-Item docs\CHECKLIST.md $stage
    Copy-Item viewer\index.html, viewer\sample-sessions.json (Join-Path $stage 'viewer')

    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
    Remove-Item -Recurse -Force $stage
    Write-Host "Created $zip"
}
finally {
    Pop-Location
}
