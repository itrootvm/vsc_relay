#Requires -Version 5.1
param([Parameter(Position = 0)][string]$Action = "status")

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Bin = Join-Path $Root "target\release\vsc-relay-agent.exe"

if (-not (Test-Path $Bin)) {
    Write-Output "build first: cargo build --release -p relay-agent -p relay-shim"
    exit 1
}

switch ($Action.ToLower()) {
    "install" { & $Bin shim-install }
    "uninstall" { & $Bin shim-uninstall }
    "status" { & $Bin shim-status }
    default { Write-Output "usage: .\shim.ps1 {install|uninstall|status}" }
}
