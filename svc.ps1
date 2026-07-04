#Requires -Version 5.1
param([Parameter(Position = 0)][string]$Action = "status")

$ErrorActionPreference = "Stop"
$RelayDir = Join-Path $env:USERPROFILE ".vsc-relay"
$PidFile = Join-Path $RelayDir "agent.pid"
$LogFile = Join-Path $RelayDir "agent.log"
$ErrFile = Join-Path $RelayDir "agent.err.log"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Bin = Join-Path $Root "target\release\vsc-relay-agent.exe"

if (-not (Test-Path $RelayDir)) { New-Item -ItemType Directory -Path $RelayDir | Out-Null }

function Get-DaemonProc {
    Get-CimInstance Win32_Process -Filter "Name = 'vsc-relay-agent.exe'" |
        Where-Object { $_.CommandLine -notmatch '\shook\s' }
}

function Start-Daemon {
    $existing = @(Get-DaemonProc)
    if ($existing.Count -gt 0) {
        Write-Output ("running (pid " + ($existing.ProcessId -join ' ') + ")")
        return
    }
    if (-not (Test-Path $Bin)) {
        Write-Output "building release..."
        & cargo build --release -p relay-agent
    }
    $p = Start-Process -FilePath $Bin -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $LogFile -RedirectStandardError $ErrFile
    $p.Id | Out-File -FilePath $PidFile -Encoding ascii
    Start-Sleep -Seconds 1
    Show-Status
}

function Stop-Daemon {
    $procs = @(Get-DaemonProc)
    if ($procs.Count -eq 0) {
        Write-Output "not running"
        if (Test-Path $PidFile) { Remove-Item $PidFile -Force }
        return
    }
    foreach ($proc in $procs) {
        try { Stop-Process -Id $proc.ProcessId -Force -ErrorAction Stop } catch {}
    }
    if (Test-Path $PidFile) { Remove-Item $PidFile -Force }
    Write-Output "stopped"
}

function Show-Status {
    $procs = @(Get-DaemonProc)
    if ($procs.Count -gt 0) {
        Write-Output ("running (pid " + ($procs.ProcessId -join ' ') + ")")
    } else {
        Write-Output "not running"
    }
    Write-Output "log: $LogFile"
    if (Test-Path $LogFile) { Get-Content $LogFile -Tail 12 }
}

switch ($Action.ToLower()) {
    "start" { Start-Daemon }
    "stop" { Stop-Daemon }
    "restart" { Stop-Daemon; Start-Sleep -Seconds 1; Start-Daemon }
    "status" { Show-Status }
    "logs" { if (Test-Path $LogFile) { Get-Content $LogFile -Tail 50 -Wait } else { Write-Output "no log yet" } }
    default { Write-Output "usage: .\svc.ps1 {start|stop|restart|status|logs}" }
}
