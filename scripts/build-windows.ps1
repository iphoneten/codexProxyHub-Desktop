$ErrorActionPreference = "Stop"

$AppName = "recodexProxyHub"
$BinName = "recodex-proxy-hub"
$Target = "x86_64-pc-windows-msvc"
$RootDir = Split-Path -Parent $PSScriptRoot
$DistDir = Join-Path $RootDir "dist"
$PackageName = "$AppName-windows-x86_64"
$PackageDir = Join-Path $DistDir $PackageName
$ZipPath = Join-Path $DistDir "$PackageName.zip"
$ExePath = Join-Path $RootDir "target\$Target\release\$BinName.exe"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found. Install Rust first: https://rustup.rs/"
}

$RawVersion = $env:RECODEX_VERSION
if ([string]::IsNullOrWhiteSpace($RawVersion) -and $env:GITHUB_REF_TYPE -eq "tag") {
    $RawVersion = $env:GITHUB_REF_NAME
}
if ([string]::IsNullOrWhiteSpace($RawVersion)) {
    $RawVersion = git -C $RootDir describe --tags --exact-match HEAD 2>$null
}
if ([string]::IsNullOrWhiteSpace($RawVersion)) {
    $CargoToml = Get-Content (Join-Path $RootDir "Cargo.toml") -Raw
    $VersionMatch = [regex]::Match(
        $CargoToml,
        '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"'
    )
    if (-not $VersionMatch.Success) {
        throw "Unable to read package version from Cargo.toml"
    }
    $RawVersion = $VersionMatch.Groups[1].Value
}

$Version = $RawVersion -replace '^v', ''
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Invalid release version '$RawVersion'. Expected tag format: v1.2.3"
}
$env:RECODEX_VERSION = $Version

$InstalledTargets = rustup target list --installed
if ($InstalledTargets -notcontains $Target) {
    throw "Rust target is not installed: $Target. Run: rustup target add $Target"
}

Push-Location $RootDir
try {
    cargo build --release --target $Target
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    Remove-Item $PackageDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $ZipPath -Force -ErrorAction SilentlyContinue
    New-Item $PackageDir -ItemType Directory -Force | Out-Null

    Copy-Item $ExePath (Join-Path $PackageDir "$AppName.exe")
    if (Test-Path (Join-Path $RootDir "config.yaml")) {
        Copy-Item (Join-Path $RootDir "config.yaml") (Join-Path $PackageDir "config.yaml")
    }
    elseif (Test-Path (Join-Path $RootDir "config.example.yaml")) {
        Copy-Item (Join-Path $RootDir "config.example.yaml") (Join-Path $PackageDir "config.yaml")
    }
    Copy-Item (Join-Path $RootDir "README.md") (Join-Path $PackageDir "README.md")
    Set-Content (Join-Path $PackageDir "VERSION.txt") -Value $Version -NoNewline

    Compress-Archive -Path "$PackageDir\*" -DestinationPath $ZipPath -CompressionLevel Optimal
    Write-Host "Windows package created: $ZipPath"
    Write-Host "Version: $Version"
}
finally {
    Pop-Location
}
