# Changelog

## Unreleased

- Windows 10/11 support alongside macOS and Linux. The daemon, shim, discovery, GUI, and
  Telegram control all run on Windows (x64). A new `relay-ipc` crate carries the IPC transport
  as Windows named pipes (per-user DACL, reject-remote-clients, first-instance guard) on
  Windows and Unix domain sockets on macOS/Linux, so the background shim path (send, answer
  questions, permissions, model/effort/mode) stays display-independent.
- New `relay-control` Windows backend for window focus and GUI fallback via Win32
  (`EnumWindows` / `SetForegroundWindow` + `AttachThreadInput` / `SendInput` / clipboard /
  `ShellExecuteW`); needs an interactive desktop session.
- Windows shim wraps `claude.exe` (moved aside to `claude.real.exe`), spawns the real helper
  via CreateProcess with a spawn-and-wait fail-safe (no `exec`), and confines it to a Job
  Object so a killed shim cannot orphan the child.
- The agent loads `relay.env` from the platform config dir (`%APPDATA%\vsc-relay` on Windows,
  `~/.config/vsc-relay` on Unix) via dotenvy, and prevents a duplicate Telegram poller with a
  single-instance named mutex on Windows.
- Windows packaging: `build_windows.ps1` produces `dist/vsc-relay-<ver>-windows-x86_64.zip`
  with per-user (no-admin) `install.ps1` / `uninstall.ps1` that wire the Claude Code hooks,
  install the shim, and register a logon Scheduled Task in the interactive session. Windows
  self-update swaps the running `.exe` aside to `.old`. `release.yml` builds and publishes it.
- CI now runs fmt/clippy/test on macOS, Linux, and Windows.
- Linux support alongside macOS. The daemon, shim, discovery, and Telegram control all run
  on Linux; the shim background path (send, answer questions, permissions, model/effort/mode)
  is display-independent.
- New Linux desktop app `vsc-relay-gui` (egui, runs on any desktop environment): token /
  pairing-key settings, start/stop, live log, sessions/turns/shim/version dashboard, shim
  install, and launch-at-login via an XDG autostart entry. Config lives in the same
  `~/.config/vsc-relay/relay.env` the systemd service reads.
- New `relay-control` Linux backend for window focus and GUI fallback via `xdotool` /
  `xclip` / `xdg-open` on X11 (XWayland tolerated; native Wayland limited to the shim path),
  with VS Code / Insiders / VSCodium / Cursor / Code-OSS / Windsurf window matching.
- Shim discovery now scans multiple editor extension roots (`.vscode`, `.vscode-insiders`,
  `.vscode-oss`, `.vscodium`, `.cursor`, `.windsurf`).
- Linux packaging: `build_linux.sh` produces a static (musl) `dist/*.tar.gz` **and a `.deb`**
  (Debian/Ubuntu, `sudo apt install ./vsc-relay_*.deb`) with a systemd user service, desktop
  launcher, and `install.sh` / `uninstall.sh`; `release.sh` is now OS-aware.
- Linux self-update: `vsc-relay-agent self-update [--check]` downloads the latest release,
  verifies its sha256, atomically swaps the binaries, and re-wraps the shim. The GUI surfaces
  it as an update banner plus an **Auto-update** toggle in Settings.
- Multiple machines: run one bot per machine for now (each token is separate, so no 409); a
  single-bot multi-machine hub is on the roadmap.
- CI builds and publishes the macOS dmg, the Linux tarball, and the `.deb` from a single
  tagged release, after a shared fmt/clippy/test/audit gate.

## 0.1.4

- Clarified the public README with purpose, support matrix, install flow, Telegram commands,
  security model, and roadmap.
- Added security, contribution, architecture, installation, troubleshooting, FAQ, Codex, and
  threat-model documentation.
- Added CI for formatting, clippy, tests, and dependency audit.
- Added release provenance steps for DMG verification, checksums, SPDX SBOM, and GitHub
  artifact attestation.
- Synchronized release version signals across `VERSION`, Cargo workspace metadata, app
  bundle metadata, and release scripts.
