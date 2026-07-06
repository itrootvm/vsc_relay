# Troubleshooting

## Telegram Says Unauthorized

Send `/auth <key>` again from the private chat you want to use. Make sure the key matches
the app setting or `RELAY_PAIR_SECRET` in `.env`.

## No VS Code Windows Appear

Open the workspace in VS Code and start a Claude Code or Codex chat. Then run `/windows` or
open `/menu` again.

## The Relay Can Read But Cannot Send

For Claude Code, install the shim and start a new Claude Code chat after installation.
Already-open chats keep using the helper binary they started with.

For Codex, this is expected. Codex background control is not implemented yet.

## Focus Does Not Work

On macOS, grant Accessibility permission to the app or terminal that runs the relay. Then
restart the relay and try `/focus <workspace>` again.

On Linux, window focus and GUI fallback need an X11 (or XWayland) session and `xdotool`
(`sudo apt install xdotool xclip xdg-utils`). Under a native Wayland session the compositor
usually blocks key injection into other windows; prefer an X11/Xorg session for the GUI
fallback. If the relay runs as a systemd user service, it must inherit `DISPLAY`. The
installer runs `systemctl --user import-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY
XDG_SESSION_TYPE`; re-run it and restart the service if you started the session differently. The background shim path does
not need a display and keeps working regardless, so sending, answering questions,
permissions, and model/effort/mode work even when `/focus` does not.

On Windows, window focus and GUI fallback need an interactive desktop session. The logon task
the installer registers runs there; a Session-0 service cannot focus windows. The background
shim path over named pipes needs no session and keeps working, so sending, answering
questions, permissions, and model/effort/mode work even when `/focus` does not.

## The Shim Looks Broken

On macOS and Linux:

```bash
./shim.sh status
./shim.sh uninstall
```

On Windows:

```powershell
.\shim.ps1 status
.\shim.ps1 uninstall
```

Restart VS Code after uninstalling. If the Claude Code extension still fails to start,
reinstall or update the extension.

## DMG Will Not Open

The app is ad-hoc signed. On first launch, right click `VSCRelay.app` and choose Open. If
the downloaded file looks corrupted, download it again and compare its SHA256 checksum with
the release `SHA256SUMS` file.
