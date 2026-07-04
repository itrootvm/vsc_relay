#Requires -Version 5.1
param([string]$Target = "x86_64-pc-windows-msvc")
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $Root

if ($env:APP_VERSION) {
    $Version = $env:APP_VERSION
} elseif (Test-Path VERSION) {
    $Version = (Get-Content VERSION -Raw).Trim()
} else {
    $Version = "0.0.0"
}
$Arch = ($Target -split '-')[0]

Write-Output "1/4 building agent + shim + gui (target $Target, version $Version)"
$ErrorActionPreference = "Continue"
rustup target add $Target 2>&1 | Out-Null
cargo build --release --target $Target -p relay-agent -p relay-shim -p relay-gui
$buildExit = $LASTEXITCODE
$ErrorActionPreference = "Stop"
if ($buildExit -ne 0) { throw "cargo build failed" }

$OutDir = Join-Path "target\$Target" "release"
$Stage = "dist\vsc-relay-$Version-windows-$Arch"
$Zip = "dist\vsc-relay-$Version-windows-$Arch.zip"

Write-Output "2/4 staging -> $Stage"
if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $Stage "bin") | Out-Null
foreach ($exe in @("vsc-relay-agent.exe", "vsc-claude-shim.exe", "vsc-relay-gui.exe")) {
    Copy-Item (Join-Path $OutDir $exe) (Join-Path (Join-Path $Stage "bin") $exe) -Force
}
Copy-Item "packaging\windows\install.ps1" (Join-Path $Stage "install.ps1") -Force
Copy-Item "packaging\windows\uninstall.ps1" (Join-Path $Stage "uninstall.ps1") -Force
Copy-Item "packaging\windows\relay.env.example" (Join-Path $Stage "relay.env.example") -Force
$Version | Out-File -FilePath (Join-Path $Stage "VERSION") -Encoding ascii

$readme = @"
VS Code Agent Relay $Version - Windows 10/11 (x64)

Setup:
  powershell -ExecutionPolicy Bypass -File .\install.ps1
  notepad %APPDATA%\vsc-relay\relay.env    (set TELEGRAM_BOT_TOKEN and RELAY_PAIR_SECRET)
  %LOCALAPPDATA%\Programs\vsc-relay\vsc-relay-agent.exe
Then in Telegram: /auth <pairing key> then /menu

Or run the desktop app: %LOCALAPPDATA%\Programs\vsc-relay\vsc-relay-gui.exe

The background shim path (send prompts, answer questions, permissions, model/mode)
needs no display. Window focus / GUI fallback needs an interactive desktop session.

Uninstall: powershell -ExecutionPolicy Bypass -File .\uninstall.ps1
"@
$readme | Out-File -FilePath (Join-Path $Stage "README.txt") -Encoding ascii

Write-Output "3/4 zipping -> $Zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Compress-Archive -Path $Stage -DestinationPath $Zip -Force

Write-Output "4/4 checksum"
$hash = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()
"$hash  $(Split-Path -Leaf $Zip)" | Out-File -FilePath "$Zip.sha256" -Encoding ascii
Write-Output $hash

Remove-Item $Stage -Recurse -Force
Write-Output "done: $Zip"
