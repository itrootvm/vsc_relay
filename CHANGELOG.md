# Changelog

## Unreleased

## 0.4.0 - 2026-07-07

- Fixed a double render of AskUserQuestion on tapped sessions. The transcript tail and the
  shim out socket both used to post a card for the same prompt; the two are now matched by
  tool use id, so a tapped session shows exactly one interactive card. An untapped session
  still gets its fallback card, so no prompt is dropped.
- Fixed a stall where a slow Telegram send could freeze Claude Code output inside VS Code.
  The shim now writes the editor stdout first and fans out to background readers over bounded
  per reader channels, so a slow or stuck reader can no longer block the output pump. The
  daemon reads its socket on a task separate from the Telegram send for the same reason.
- Added a replay ring in the shim. A background reader that reconnects, for example after the
  daemon restarts, is re-sent any still pending permission or question request, and answered
  requests are evicted, so a reconnect does not lose or duplicate a card. A reader disconnect
  while the session process is still alive is treated as a reconnect window rather than a
  closed session, so a pending card stays answerable.
- Added permission mode change alerts. A switch into acceptEdits or bypassPermissions raises
  an alert in Telegram; other transitions are shown without one.
- Added per turn token accounting for both Claude Code and Codex (input, output, cache read,
  and cache creation), recorded per session.
- Telegram answers and permission decisions are no longer injected into a session that has
  closed or whose pid has been reused. The pending card carries its session identity and the
  inject is skipped on a mismatch.
- Untapped sessions now show a card that points to VS Code instead of option buttons that
  could not answer the prompt.
- Auto reshim repairs a freshly updated extension version without a restart. Install status is
  tracked per extension directory, so a new version is tapped even when an older one is already
  shimmed.
- Added detailed lifecycle logging across both pipelines (arrived, sent, pressed, answered in
  VS Code, resolved, suppressed, mode changed) with per card latency and request and response
  byte sizes. Logs redact the bot token, tool input, option labels, and raw commands.
- Split the callback and full text stores so a burst of full text previews can no longer evict
  an active permission or send callback.

## 0.3.0 - 2026-07-05

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

## 0.2.0

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

## 0.1.5

- Bidirectional permission forwarding: permission prompts for every tool (Workflow, Artifact,
  Skill, Bash, AskUserQuestion, and the rest) now reach Telegram, and answering on one side
  clears the other. A Telegram answer dismisses the VS Code menu via a control_cancel_request;
  answering in VS Code voids the Telegram card.
- Redacted logging on the permission and hook paths that records only the tool and a byte
  count, never command arguments, file contents, or URLs beyond scheme and host.

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
