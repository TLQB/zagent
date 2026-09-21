[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][Alias('o')][string]$OutputDir
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

# Detect architecture
$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) { $Architecture } else { $OSArchitecture }
$target = "$Architecture-pc-windows-msvc"
$CargoOutDir = "./target/$Architecture-pc-windows-msvc/release"

# Set workspace
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$workspace = Split-Path -Parent $scriptDir
$env:ZED_WORKSPACE = $workspace

# Read release channel
Push-Location
Set-Location "$workspace/crates/zed"
$channel = if (Test-Path "RELEASE_CHANNEL") { Get-Content "RELEASE_CHANNEL" } else { "dev" }
Pop-Location

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Cognix Portable Windows Builder" -ForegroundColor Cyan
Write-Host "  Architecture: $Architecture" -ForegroundColor Cyan
Write-Host "  Channel: $channel" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

# Set up VS Dev Shell
function Get-VSArch {
    param([string]$Arch)
    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

$vsDevShell = "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\Launch-VsDevShell.ps1"
if (Test-Path $vsDevShell) {
    Write-Host "[1/7] Setting up VS Dev Shell..." -ForegroundColor Yellow
    Push-Location
    & $vsDevShell -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
    Pop-Location
} else {
    Write-Host "[1/7] VS Dev Shell not found, using existing environment" -ForegroundColor Yellow
}

# Create output directory
$portableDir = "$workspace/target/cognix-portable-$Architecture"
if (Test-Path $portableDir) {
    Remove-Item -Path $portableDir -Recurse -Force
}
New-Item -Path $portableDir -ItemType Directory -Force | Out-Null
New-Item -Path "$portableDir/bin" -ItemType Directory -Force | Out-Null
New-Item -Path "$portableDir/x64" -ItemType Directory -Force | Out-Null
New-Item -Path "$portableDir/arm64" -ItemType Directory -Force | Out-Null

# [2/7] Build binaries
Write-Host "[2/7] Building zed.exe, cli.exe, auto_update_helper.exe..." -ForegroundColor Yellow
cargo build --release --package zed --package cli --package auto_update_helper --target $target

if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo build failed"
    exit 1
}

Copy-Item -Path "$CargoOutDir/zed.exe" -Destination "$portableDir/Cognix.exe" -Force
Copy-Item -Path "$CargoOutDir/cli.exe" -Destination "$portableDir/cli.exe" -Force
Copy-Item -Path "$CargoOutDir/auto_update_helper.exe" -Destination "$portableDir/tools/auto_update_helper.exe" -Force

# [3/7] Build explorer_command_injector.dll
Write-Host "[3/7] Building explorer_command_injector.dll..." -ForegroundColor Yellow
cargo build --release --package explorer_command_injector --target $target

if ($LASTEXITCODE -ne 0) {
    Write-Host "  Warning: explorer_command_injector build failed, continuing..." -ForegroundColor Yellow
} else {
    Copy-Item -Path "$CargoOutDir/explorer_command_injector.dll" -Destination "$portableDir/zed_explorer_command_injector.dll" -Force
}

# [4/7] Download ConPTY
Write-Host "[4/7] Downloading ConPTY (Microsoft Terminal backend)..." -ForegroundColor Yellow
$conptyNupkg = "Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
$conptyUrl = "https://github.com/microsoft/terminal/releases/download/v1.23.13503.0/$conptyNupkg"
$conptyZip = "$env:TEMP/$conptyNupkg"
$conptyExtract = "$env:TEMP/conpty_extract"

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $conptyUrl -OutFile $conptyZip -UseBasicParsing
    Write-Host "  Downloaded ConPTY nupkg" -ForegroundColor Green

    if (Test-Path $conptyExtract) {
        Remove-Item -Path $conptyExtract -Recurse -Force
    }
    Expand-Archive -Path $conptyZip -DestinationPath $conptyExtract -Force
    Write-Host "  Extracted ConPTY" -ForegroundColor Green

    # Copy architecture-specific files
    if ($Architecture -eq "aarch64") {
        $openConsoleSrc = "$conptyExtract/build/native/runtimes/arm64/OpenConsole.exe"
        $conptyDllSrc = "$conptyExtract/runtimes/win-arm64/native/conpty.dll"
    } else {
        $openConsoleSrc = "$conptyExtract/build/native/runtimes/x64/OpenConsole.exe"
        $conptyDllSrc = "$conptyExtract/runtimes/win-x64/native/conpty.dll"
    }

    if (Test-Path $openConsoleSrc) {
        Copy-Item -Path $openConsoleSrc -Destination "$portableDir/x64/OpenConsole.exe" -Force
        Write-Host "  Copied OpenConsole.exe" -ForegroundColor Green
    } else {
        Write-Host "  Warning: OpenConsole.exe not found at $openConsoleSrc" -ForegroundColor Yellow
    }

    if (Test-Path $conptyDllSrc) {
        Copy-Item -Path $conptyDllSrc -Destination "$portableDir/conpty.dll" -Force
        Write-Host "  Copied conpty.dll" -ForegroundColor Green
    } else {
        Write-Host "  Warning: conpty.dll not found at $conptyDllSrc" -ForegroundColor Yellow
    }
} catch {
    Write-Host "  Warning: Failed to download ConPTY: $_" -ForegroundColor Yellow
    Write-Host "  Terminal will work with reduced functionality" -ForegroundColor Yellow
}

# [5/7] Create cli.exe symlink (zed command)
Write-Host "[5/7] Creating CLI symlink..." -ForegroundColor Yellow
# On Windows, we create a batch file wrapper instead of symlink
$cliBat = @"
@echo off
"%~dp0\cli.exe" %*
"@
Set-Content -Path "$portableDir/bin/zed.bat" -Value $cliBat -Encoding ASCII
Write-Host "  Created bin\zed.bat" -ForegroundColor Green

# [6/7] Create README
Write-Host "[6/6] Creating README..." -ForegroundColor Yellow
$readme = @"
Cognix Portable - Windows
========================

Quick Start:
  1. Extract this zip to any folder
  2. Run Cognix.exe directly
  3. Or add the 'bin' folder to your PATH and run 'zed' from command line

Files:
  Cognix.exe           - Main editor
  cli.exe              - CLI tool
  conpty.dll           - Windows Terminal backend (ConPTY)
  x64\OpenConsole.exe  - Console host
  bin\zed.bat          - CLI wrapper script
  tools\                - Helper tools

Requirements:
  - Windows 10 (1903+) or Windows 11
  - Visual C++ Redistributable 2015-2022 (usually pre-installed)

Notes:
  - This is a portable build, no installation required
  - Settings are stored in %APPDATA%\Cognix
  - If terminal doesn't work, ensure conpty.dll is in the same folder as Cognix.exe
"@
Set-Content -Path "$portableDir/README.txt" -Value $readme -Encoding UTF8

# [7/7] Create zip
Write-Host "[7/7] Creating portable zip..." -ForegroundColor Yellow
$zipName = "cognix-$Architecture-windows-portable.zip"
$zipPath = if ($OutputDir) { "$OutputDir/$zipName" } else { "$workspace/target/$zipName" }

if (Test-Path $zipPath) {
    Remove-Item -Path $zipPath -Force
}

Compress-Archive -Path "$portableDir/*" -DestinationPath $zipPath -Force

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "  Build Complete!" -ForegroundColor Green
Write-Host "  Output: $zipPath" -ForegroundColor Green
Write-Host "  Size: $([math]::Round((Get-Item $zipPath).Length / 1MB, 2)) MB" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
