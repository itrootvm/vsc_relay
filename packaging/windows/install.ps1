#Requires -Version 5.1
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$SrcBin = if (Test-Path (Join-Path $ScriptDir "bin")) { Join-Path $ScriptDir "bin" } else { $ScriptDir }

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\vsc-relay"
$ConfigDir = Join-Path $env:APPDATA "vsc-relay"
$Agent = Join-Path $InstallDir "vsc-relay-agent.exe"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

foreach ($exe in @("vsc-relay-agent.exe", "vsc-claude-shim.exe", "vsc-relay-gui.exe")) {
    $src = Join-Path $SrcBin $exe
    if (Test-Path $src) { Copy-Item $src (Join-Path $InstallDir $exe) -Force }
}

$EnvFile = Join-Path $ConfigDir "relay.env"
if (-not (Test-Path $EnvFile)) {
    $example = Join-Path $ScriptDir "relay.env.example"
    if (Test-Path $example) {
        Copy-Item $example $EnvFile
    } else {
        "TELEGRAM_BOT_TOKEN=`r`nRELAY_PAIR_SECRET=`r`n" | Out-File -FilePath $EnvFile -Encoding ascii
    }
}
try { & icacls $EnvFile /inheritance:r /grant:r "$($env:USERNAME):F" | Out-Null } catch {}

if (Test-Path $Agent) {
    try { & $Agent install-hooks } catch { Write-Output "install-hooks: $($_.Exception.Message)" }
    try { & $Agent shim-install } catch { Write-Output "shim-install: $($_.Exception.Message)" }
}

$taskName = "VSCRelay"
try {
    $action = New-ScheduledTaskAction -Execute $Agent
    $trigger = New-ScheduledTaskTrigger -AtLogOn
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
    Write-Output "registered logon task '$taskName' (runs in your interactive session)"
} catch {
    Write-Output "could not register scheduled task: $($_.Exception.Message)"
}

Write-Output ""
Write-Output "installed to $InstallDir"
Write-Output "1. edit $EnvFile (set TELEGRAM_BOT_TOKEN and RELAY_PAIR_SECRET)"
Write-Output "2. start now:  & `"$Agent`""
Write-Output "3. in Telegram: /auth <key> then /menu"
