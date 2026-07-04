#Requires -Version 5.1
$ErrorActionPreference = "Continue"

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\vsc-relay"
$Agent = Join-Path $InstallDir "vsc-relay-agent.exe"
$taskName = "VSCRelay"

try {
    Get-CimInstance Win32_Process -Filter "Name = 'vsc-relay-agent.exe'" |
        Where-Object { $_.CommandLine -notmatch '\shook\s' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
} catch {}

if (Test-Path $Agent) {
    try { & $Agent shim-uninstall } catch {}
}

try { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction Stop } catch {}

if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue }

Write-Output "uninstalled binaries and logon task."
Write-Output "config kept at $(Join-Path $env:APPDATA 'vsc-relay'); remove it manually to fully clean up."
