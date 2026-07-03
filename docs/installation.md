# Installation

The macOS app is the simplest path on a Mac. On Linux, use the release tarball with a
systemd user service. Both platforms share the same daemon, shim, and terminal helpers.

## App Install (macOS)

1. Download `VSCRelay.dmg` from the latest GitHub release.
2. Open the disk image.
3. Drag `VSCRelay.app` to Applications.
4. Launch the app. On first launch, right click and choose Open if macOS blocks it.
5. Open Settings.
6. Paste your Telegram bot token from BotFather.
7. Set a pairing key.
8. Click Start.
9. In Telegram, send `/auth <key>` to your bot.
10. Send `/menu`.

The app stores the bot token and pairing key in Keychain.

## Linux Install

Linux has a GUI app (`vsc-relay-gui`, built with egui, runs on any desktop environment) and
a headless systemd service. Use whichever you prefer, but not both at once.

On Debian or Ubuntu, install the `.deb` (it pulls the GUI's library dependencies via apt):

```bash
sudo apt install ./vsc-relay_<version>_amd64.deb
```

It installs the binaries to `/usr/bin`, a desktop launcher, and a `systemctl --user`
service unit. Configuration and per-user setup happen when you first run the GUI.

On any other distribution, use the static tarball:

1. Download `vsc-relay-<version>-linux-<arch>.tar.gz` from the latest GitHub release and
   unpack it. The daemon and shim are statically linked (musl), so they run on any
   distribution.
2. Run the installer:

   ```bash
   ./install.sh
   ```

   It installs `vsc-relay-agent`, `vsc-claude-shim`, and `vsc-relay-gui` to `~/.local/bin`,
   a desktop launcher, a systemd user service to `~/.config/systemd/user`, and an
   environment file at `~/.config/vsc-relay/relay.env`, and wires the Claude Code hooks.

### GUI path

Launch **VS Code Agent Relay** from your app menu (or run `~/.local/bin/vsc-relay-gui`).
Open Settings, paste your Telegram bot token, set a pairing key (Generate makes one), and
click Start. Use **Install shim** for background control. The app shows sessions/turns today,
shim status, the Claude Code version, and a live log, and has a Launch-at-login toggle (an
XDG autostart entry). Then in Telegram, send `/auth <key>` to your bot and `/menu`.

### Headless (systemd) path

```bash
$EDITOR ~/.config/vsc-relay/relay.env
~/.local/bin/vsc-relay-agent shim-install
systemctl --user enable --now vsc-relay
loginctl enable-linger "$USER"   # optional: keep running after logout
```

Status and logs:

```bash
systemctl --user status vsc-relay
journalctl --user -u vsc-relay -f
```

Window focus and GUI fallback need an X11 (or XWayland) session plus `xdotool`:

```bash
sudo apt install xdotool xclip xdg-utils   # Debian/Ubuntu
sudo dnf install xdotool xclip xdg-utils   # Fedora
```

The background shim path (send, answer questions, permissions, model/effort/mode) works
without any of those and without a display.

Do not run both the systemd service and `./svc.sh start`; two Telegram pollers collide
with a 409.

### Updating on Linux

The GUI checks GitHub on launch (and every six hours) and shows an Update banner when a newer
release exists. Click **Update now**, or enable **Auto-update** in Settings. Under the hood it
runs the same self-update the CLI exposes:

```bash
vsc-relay-agent self-update --check   # report current vs latest
vsc-relay-agent self-update           # download the release tarball, verify its
                                      # sha256, atomically swap the binaries, re-wrap
                                      # the shim, then restart the app/service to apply
```

The self-update replaces `vsc-relay-agent`, `vsc-claude-shim`, and `vsc-relay-gui` next to
the running agent. For a `.deb` install, updating with `sudo apt install ./<newer>.deb`
works too. A checksum mismatch aborts the swap.

### Multiple machines

Each machine runs its own bot for now: create a separate bot token per machine (label them
with `RELAY_MACHINE_NAME`). A single-bot hub that fans out to every machine is on the
roadmap.

## Terminal Service

```bash
cp .env.example .env
./svc.sh start
./svc.sh status
./svc.sh logs
```

Set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in `.env` before starting the service.

## Shim Install

Install the Claude Code shim if you want background prompts and question answers:

```bash
./shim.sh install
./shim.sh status
```

Only Claude Code chats started after shim installation use the background control channel.

Uninstall the shim:

```bash
./shim.sh uninstall
```

## Build From Source

macOS (needs Rust stable and the Xcode command line tools):

```bash
./build_app.sh
```

The build creates `build/VSCRelay.app` and, with `hdiutil`, `build/VSCRelay.dmg`.
Packaged macOS builds target Apple Silicon; universal Intel support is planned.

Linux (needs Rust stable; `musl-tools` for a portable static build):

```bash
./build_linux.sh
```

It builds the daemon and shim, preferring the static `x86_64-unknown-linux-musl` target
when available and falling back to the host `gnu` target, and stages
`dist/vsc-relay-<version>-linux-<arch>.tar.gz` with the installer and helpers.

To just build the binaries on either platform:

```bash
cargo build --release
```
