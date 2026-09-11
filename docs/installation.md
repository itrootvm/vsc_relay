# Installation

The macOS app is the simplest path on a Mac. On Linux, use the `.deb` or the release tarball
with a `systemd --user` service. On Windows, use the zip installer, which registers a logon
Scheduled Task. All three platforms share the same daemon, shim, and terminal helpers.

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

To supervise a dev clone instead of the packaged app, `./launchd.sh install` writes a
`dev.vscrelay.agent` LaunchAgent for `target/release/vsc-relay-agent`; `./launchd.sh status`
and `./launchd.sh uninstall` round it out.

## Linux Install

Linux ships three binaries: `vsc-relay-agent` (the daemon), `vsc-claude-shim` (the Claude Code
shim), and `vsc-relay-gui` (an egui desktop app that runs on any desktop environment, X11 or
Wayland). Run the GUI or the systemd service, not both.

How they are linked decides which package you want:

- `vsc-relay-agent` and `vsc-claude-shim` are statically linked (musl) in the release build, so
  they run on any distribution.
- `vsc-relay-gui` is **not** static. It links dynamically against the system GL, X11, Wayland,
  xkbcommon and fontconfig libraries. That is why the `.deb` carries a `Depends` list, and why a
  tarball install needs those libraries already present. Release GUI builds are made on
  ubuntu-22.04 to keep the glibc floor low; the daemon and shim are unaffected because they are
  static.

### Debian or Ubuntu (.deb)

```bash
sudo apt install ./vsc-relay_<version>_amd64.deb
```

It installs:

- `/usr/bin/vsc-relay-agent`, `/usr/bin/vsc-claude-shim`, `/usr/bin/vsc-relay-gui`
- `/usr/lib/systemd/user/vsc-relay.service`
- `/usr/share/applications/vsc-relay.desktop` (the "VS Code Agent Relay" launcher)
- `/usr/share/doc/vsc-relay/relay.env.example`

`Depends` pulls the GUI's libraries: `libc6`, `libgl1`, `libxkbcommon0`, `libxkbcommon-x11-0`,
`libwayland-client0`, `libfontconfig1`, `libxcursor1`, `libxi6`, `libxrandr2`. `Recommends` adds
`xdotool`, `xclip` and `xdg-utils` for window focus. Configuration and per-user setup (env file,
Claude Code hooks, shim) happen when you first run the GUI, or by hand as in the headless path
below.

### Any other distribution (tarball)

1. Download `vsc-relay-<version>-linux-<arch>.tar.gz` from the latest GitHub release and
   unpack it. The daemon and shim are statically linked (musl), so they run on any
   distribution; the bundled GUI still needs the system libraries listed above.
2. Run the installer:

   ```bash
   ./install.sh
   ```

   It installs `vsc-relay-agent`, `vsc-claude-shim`, and `vsc-relay-gui` to `~/.local/bin`, a
   desktop launcher to `~/.local/share/applications`, a systemd user service to
   `~/.config/systemd/user/vsc-relay.service`, and an environment file at
   `~/.config/vsc-relay/relay.env` (mode 0600 in a 0700 directory, kept as-is if it already
   exists), wires the Claude Code hooks with `vsc-relay-agent install-hooks`, runs
   `systemctl --user daemon-reload`, and imports `DISPLAY`, `XAUTHORITY`, `WAYLAND_DISPLAY` and
   `XDG_SESSION_TYPE` into the user manager so a service-started relay can reach your desktop.

The tarball also carries `uninstall.sh`, which disables the unit, restores the shim, and removes
the binaries, unit and launcher; add `--purge` to delete `~/.config/vsc-relay` and `~/.vsc-relay`
too.

### GUI path

Launch **VS Code Agent Relay** from your app menu (or run `~/.local/bin/vsc-relay-gui`). Open
Settings, paste your Telegram bot token, set a pairing key (Generate makes one), and click Start.
Use **Install shim** for background control. Then in Telegram, send `/auth <key>` to your bot and
`/menu`.

The v0.5.0 app is a full control panel, not just a start button:

- Five stat cards: Sessions today, Turns today, Active, Attention (how many are waiting on you),
  and Shim.
- A status strip of Compass / Steer / Gate pills with the current semantic backend next to them,
  and a warning when uncalibrated semantic actions are being blocked.
- Session list with a search box (alias, title or id) and All / Active / Attention / Robot
  filters.
- Per-session buttons: **Cross review**, **Hand off**, **Usage**, **Gate pins**, **Copy id**.
  The Hand off window groups destinations as Claude chats, Agent CLIs and Editors.
- A Diagnostics pane that tails `~/.vsc-relay/agent.log` live, with a text filter, a **Gate trace
  only** toggle, and a **This session only** toggle once a session is selected.
- A Cross review block in Settings: enable, steer, allow a same-family reviewer, depth
  (shallow / normal / deep), cadence (5m / 15m / 30m / 1h), and reviewer checkboxes.
- Compass settings: enable, auto-steer, gate, a USD budget cap, and the semantic backend picker.
- Provider health, per-provider keys and model, `Check health`, and `Login` for CLI providers.
- A Launch-at-login toggle (an XDG autostart entry) and an Update banner.

Starting the GUI clears stray `vsc-relay-agent` processes first, so do not expect it to coexist
with the systemd service.

### Headless (systemd) path

```bash
$EDITOR ~/.config/vsc-relay/relay.env
~/.local/bin/vsc-relay-agent shim-install
systemctl --user enable --now vsc-relay
loginctl enable-linger "$USER"   # optional: keep running after logout
```

With the `.deb` the unit is already on the system at `/usr/lib/systemd/user/vsc-relay.service`
(pointing at `/usr/bin/vsc-relay-agent`), so nothing has to write one — but every line in that
block still applies. The `.deb` creates no per-user state: the unit still reads
`~/.config/vsc-relay/relay.env`, so copy `/usr/share/doc/vsc-relay/relay.env.example` there
yourself (0600 in a 0700 directory, which is what the tarball installer does) before the first
start.

Unit state:

```bash
systemctl --user status vsc-relay
```

### Dev clone under systemd

`./systemd.sh` is the Linux counterpart to `./launchd.sh`: it supervises a dev clone rather than
an installed release.

```bash
cargo build --release -p relay-agent
./systemd.sh install
./systemd.sh status
./systemd.sh logs
```

`install` writes `~/.config/systemd/user/vsc-relay.service` pointing at
`$PWD/target/release/vsc-relay-agent`, creates an empty `~/.config/vsc-relay/relay.env` (mode
0600) if there is none, imports the graphical environment variables, stops any running GUI or
daemon so the single-instance lock is free, then enables and starts the unit. `start`, `stop`,
`restart` and `uninstall` do what they say, and `logs` tails `~/.vsc-relay/agent.log`. It refuses
to run with a clear message when there is no `systemctl` or no user manager for the session.

### Reading the log

The daemon writes its own log to a **file**, not to stdout, so `journalctl` shows almost nothing
from it. Read the file:

```bash
tail -f ~/.vsc-relay/agent.log
```

It rotates itself at 2 MB and keeps twelve generations (`agent.log.1` … `agent.log.12`). The
GUI's Diagnostics pane tails the same file. `journalctl --user -u vsc-relay` is still worth a
look for unit lifecycle: start, stop, restart and crash lines.

### PATH under systemd

`systemctl --user show-environment` has a minimal PATH (`/usr/local/bin:/usr/bin:/bin`, plus a
couple more), with no `~/.local/bin` and no nvm-installed node. That used to make CLI agents
invisible to a service-started relay. The agent now searches, in addition to PATH:
`~/.local/bin`, `~/bin`, `~/.npm-global/bin`, `$NPM_CONFIG_PREFIX/bin`, `~/.volta/bin`,
`~/.bun/bin`, `~/.deno/bin`, `~/.cargo/bin`, `~/.yarn/bin`, the newest
`~/.nvm/versions/node/*/bin`, `/usr/local/bin`, `/opt/homebrew/bin` and `/snap/bin`, and it
passes that augmented PATH (plus `TERM`) to every CLI agent it spawns. Node-shebang CLIs work
under the service without editing the unit.

Check what it can see:

```bash
vsc-relay-agent automation discover
vsc-relay-agent env-check
```

### Semantic backend on Linux

The compass can use a semantic backend to read transcripts. **Linux builds have no local ONNX
backend.** `relay-semantic` gates `ort` and `tokenizers` behind a `local-onnx` cargo feature that
is additionally switched off for `target_os = "linux"`, because the prebuilt ONNX Runtime needs
glibc 2.38 or newer and has no musl build at all — shipping it would end "runs on any
distribution". So the default backend on Linux is `off`, not `local`.

Selecting `local` anyway is accepted by the config commands and by the GUI's "Built-in local NLI"
entry — nothing rejects the setting — but every call that would actually use it stops with
`this build has no local ONNX semantic backend ... set smart.semantic.backend ... to off, ollama,
openai_compatible or agent_cli`. So `automation smart check` reports that instead of a ready
backend, and the compass falls back to its deterministic path at run time. The two helper
commands behave the same way: `automation smart install-local` still downloads and verifies the
model files, and `automation smart train-local` still runs its trainer, but the bundle they leave
behind is only loadable by a macOS or Windows build. Treat both as macOS/Windows features.

The Linux choices are `off`, `ollama`, `openai_compatible` and `agent_cli`. Set one with:

```bash
vsc-relay-agent automation smart provider ollama
vsc-relay-agent automation smart model <model id you have pulled>
vsc-relay-agent automation smart check
```

`ollama` points at `http://localhost:11434` and stays on this machine. The other presets send
transcript text off the machine, and the command prints a warning saying so when you pick one:

```bash
vsc-relay-agent automation smart provider openrouter
vsc-relay-agent automation smart provider nvidia
vsc-relay-agent automation smart provider openai-compatible
vsc-relay-agent automation smart endpoint https://host/v1
vsc-relay-agent automation smart key -
```

`automation smart key -` reads the key from stdin; `clear` removes it. To borrow an agent CLI you
already have logged in instead of an API key, use one of the `agent_cli` presets — `claude`,
`codex`, `gemini`, `cursor` or `antigravity`:

```bash
vsc-relay-agent automation smart provider codex
```

The GUI's backend picker offers off, local, Ollama, OpenRouter, NVIDIA NIM and
OpenAI-compatible; the agent-CLI presets are set from the command line.

### Linux CLI surface

Everything the GUI does is reachable from `vsc-relay-agent`, which matters most on a headless
box:

Setup and service:

```bash
vsc-relay-agent install-hooks
vsc-relay-agent shim-install
vsc-relay-agent shim-status
vsc-relay-agent shim-uninstall
vsc-relay-agent env-check
vsc-relay-agent selftest
vsc-relay-agent self-update
```

Inspection:

```bash
vsc-relay-agent sessions
vsc-relay-agent decisions --since 24h --by outcome
vsc-relay-agent danger-check
vsc-relay-agent send <alias> <session_id> <text>
```

Modes and rules (`automation`):

```bash
vsc-relay-agent automation get
vsc-relay-agent automation list
vsc-relay-agent automation resolve <alias> [session_id]
vsc-relay-agent automation set-default <manual|auto|robot>
vsc-relay-agent automation set-workspace <alias> <mode> [seconds]
vsc-relay-agent automation set-session <session_id> <mode> [seconds]
vsc-relay-agent automation clear-workspace <alias>
vsc-relay-agent automation clear-session <session_id>
vsc-relay-agent automation rewrite <robot|manual> <on|off>
```

Cross review:

```bash
vsc-relay-agent automation review on
vsc-relay-agent automation review off
vsc-relay-agent automation review steer <on|off>
vsc-relay-agent automation review same-family <on|off>
vsc-relay-agent automation review reviewers codex-cli,claude-cli
vsc-relay-agent automation review every <seconds>
vsc-relay-agent automation review depth <shallow|normal|deep>
vsc-relay-agent automation review budget <n>
vsc-relay-agent automation review run <session_id> [--reviewer ID] [--depth D]
vsc-relay-agent automation review probe <claude|codex> <transcript.jsonl>
```

Compass and the smart layer:

```bash
vsc-relay-agent automation smart status
vsc-relay-agent automation smart on
vsc-relay-agent automation smart off
vsc-relay-agent automation smart steer <on|off>
vsc-relay-agent automation smart gate <on|off>
vsc-relay-agent automation smart feedback <on|off>
vsc-relay-agent automation smart budget <USD|off>
vsc-relay-agent automation smart trust <on|off>
vsc-relay-agent automation smart check
vsc-relay-agent automation compass <session_id>
vsc-relay-agent automation compass-pins <session_id>
vsc-relay-agent automation compass-pin <session_id> <target> <obligation_id> <epoch>
vsc-relay-agent automation compass-unpin <session_id> [target]
vsc-relay-agent automation compass-mark <session_id> <label>
vsc-relay-agent automation compass-link <child_id> <parent_id>
vsc-relay-agent automation compass-unlink <child_id>
vsc-relay-agent automation compass-profile <status|bootstrap>
vsc-relay-agent automation compass-assess <session_id>
vsc-relay-agent automation usage <session_id>
```

Providers and routing:

```bash
vsc-relay-agent automation discover [id] [--json]
vsc-relay-agent automation health [id] [--json]
vsc-relay-agent automation models <id>
vsc-relay-agent automation login <id>
vsc-relay-agent automation provider <id> <on|off>
vsc-relay-agent automation provider-model <id> <model>
vsc-relay-agent automation provider-key <id> [VALUE|-|clear]
vsc-relay-agent automation strategy <single|priority|round_robin|cost_optimized>
vsc-relay-agent automation ask <backend> <model> <prompt>
vsc-relay-agent automation route <prompt>
vsc-relay-agent automation improve <prompt>
```

Handoff, which now works on Linux:

```bash
vsc-relay-agent handoff targets [--json]
vsc-relay-agent handoff destinations <session_id> [--json]
vsc-relay-agent handoff <session_id> --to <destination_id>
vsc-relay-agent handoff <session_id> <target_workspace>
vsc-relay-agent handoff receipt <target_workspace> [--json]
```

Editors are found on PATH (`code`, `code-insiders`, `code-oss`, `codium`, `vscodium`, `cursor`,
`windsurf`, `antigravity`) instead of by scanning `.app` bundles. A CLI handoff opens a real
terminal, trying `x-terminal-emulator`, `ptyxis`, `kgx`, `gnome-terminal`, `konsole`,
`xfce4-terminal`, `mate-terminal`, `tilix`, `terminator`, `alacritty`, `wezterm`, `kitty`,
`foot`, `lxterminal`, `deepin-terminal`, `qterminal`, `urxvt`, `st`, `xterm` in that order. With
no graphical session it fails with a message naming the launcher script, so you can run it
yourself.

Modes and window control also exist as one-shot commands: `focus`, `say`, `stop`, `cont`, `mode`
and `slash`.

### Configuration files

| Path | What it holds |
| --- | --- |
| `~/.config/vsc-relay/relay.env` | Environment for the daemon; the unit reads it with `EnvironmentFile=` |
| `~/.vsc-relay/automation.json` | Modes, auto and robot rules, cross review, compass and semantic settings |
| `~/.vsc-relay/agent.log` | The daemon's log, rotated |
| `~/.vsc-relay/decisions.jsonl` | One JSON object per decision, read by `decisions` |
| `~/.vsc-relay/vsc-relay-agent.lock` | The single-instance lock |

Keys the daemon reads from `relay.env`: `TELEGRAM_BOT_TOKEN`, `RELAY_PAIR_SECRET`,
`TELEGRAM_ALLOWED_CHATS`, `RELAY_MACHINE_NAME`, `RELAY_INTERVAL`, `RELAY_CODEX_MAX_AGE_H`,
`RELAY_MEDIA_TTL_H`, `RELAY_MEDIA_MAX_MB`, `RELAY_MEDIA_STORE_MB` and `RELAY_MEDIA_PREPROCESS`
(set it to `off`, `0`, `false` or `no` to skip media preprocessing; anything else, including
leaving it blank, keeps it on). The GUI also reads and writes `VSC_RELAY_AUTO_UPDATE` there for
its Auto-update toggle.

The shipped template — `packaging/linux/relay.env.example` in the tarball, installed as
`/usr/share/doc/vsc-relay/relay.env.example` by the `.deb` — lists every one of those plus the
`VSC_RELAY_*` entries from the table below. Only `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` have
to be filled in; `RELAY_INTERVAL`, `RELAY_CODEX_MAX_AGE_H` and `RELAY_MEDIA_TTL_H` come with their
defaults spelled out, and the rest are left blank.

Blank is not the same as absent for three of them. A `KEY=` line still puts an empty value in the
daemon's environment, and `VSC_RELAY_NO_RESHIM`, `VSC_RELAY_MEDIA_ROOT` and
`VSC_RELAY_ARTIFACT_ROOT` are tested for presence rather than for content: left in as empty lines
they switch off the startup shim refresh and point the media and artifact stores at an empty
relative path instead of `~/.vsc-relay/media` and `~/.vsc-relay/artifacts`. Delete those three
lines from your `relay.env` unless you are setting them.

Other environment variables the daemon honours, useful under systemd because the unit's
environment is otherwise minimal:

| Variable | Effect |
| --- | --- |
| `VSC_RELAY_PROXY`, `ALL_PROXY`, `HTTPS_PROXY` | Fallback route to Telegram, first non-empty wins |
| `VSC_RELAY_TELEGRAM_API` | Alternate Telegram API base |
| `VSC_RELAY_SEGMENTED_GATE` | `1`, `on` or `true` lets the segment-by-segment shell reading change gate decisions |
| `VSC_RELAY_DECISION_LOG` | Override the decision log path |
| `VSC_RELAY_NO_RESHIM` | Skip the automatic shim refresh at startup |
| `VSC_RELAY_MEDIA_ROOT`, `VSC_RELAY_ARTIFACT_ROOT` | Override where media and artifacts are stored |
| `OLLAMA_HOST`, `OPENAI_API_KEY`, `NVIDIA_API_KEY` | Picked up by provider discovery |

### Desktop dependencies

Window focus and GUI fallback need an X11 (or XWayland) session plus `xdotool`; `xclip` and
`xdg-utils` are recommended:

```bash
sudo apt install xdotool xclip xdg-utils   # Debian/Ubuntu
sudo dnf install xdotool xclip xdg-utils   # Fedora
```

The background shim path (send, answer questions, permissions, model/effort/mode) works without
any of those and without a display.

### One daemon at a time

Do not run the systemd service, the GUI, `./systemd.sh` and `./svc.sh start` together expecting
several daemons. The agent takes an exclusive single-instance lock at
`~/.vsc-relay/vsc-relay-agent.lock`; a second copy exits and names the process holding it, and the
GUI's Start clears stray daemons before launching its own. Pick one.

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

## Windows Install

Windows has the same GUI app (`vsc-relay-gui.exe`) and a background agent that starts at
logon. Windows 10 or 11 (x64) is supported.

1. Download `vsc-relay-<version>-windows-<arch>.zip` from the latest GitHub release and
   unpack it.
2. From PowerShell, run the installer:

   ```powershell
   .\install.ps1
   ```

   It copies `vsc-relay-agent.exe`, `vsc-claude-shim.exe`, and `vsc-relay-gui.exe` to
   `%LOCALAPPDATA%\Programs\vsc-relay`, writes an environment file at
   `%APPDATA%\vsc-relay\relay.env` (tightened with icacls), wires the Claude Code hooks and
   shim, and registers a `VSCRelay` logon Scheduled Task that runs in your interactive
   session.
3. Edit `%APPDATA%\vsc-relay\relay.env`, set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET`,
   then start the agent (or launch `vsc-relay-gui.exe` and click Start).
4. In Telegram, send `/auth <key>` to your bot and `/menu`.

Window focus and GUI fallback use the built-in Win32 backend and need an interactive desktop
session, which the logon task provides. The background shim path works without one.

Update in place with `vsc-relay-agent self-update` (the GUI's Update banner runs the same
command); on Windows it downloads the release `.zip`, verifies its sha256, atomically swaps
the binaries, and re-wraps the shim. Uninstall with `.\uninstall.ps1`, which stops the agent,
removes the shim and the logon task, and deletes the installed binaries; your `relay.env` is
kept.

## Terminal Service

```bash
cp .env.example .env
./svc.sh start
./svc.sh status
./svc.sh logs
```

Set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in `.env` before starting the service. This
path is for a dev clone: it builds `target/release/vsc-relay-agent` and runs it under `nohup`,
appending to `~/.vsc-relay/agent.log`, which is the same file the daemon and the GUI read.

On Windows, use `.\svc.ps1 start|status|logs` for the same daemon.

## Shim Install

Install the Claude Code shim if you want background prompts and question answers:

```bash
./shim.sh install
./shim.sh status
```

Only Claude Code chats started after shim installation use the background control channel.
On Windows, use `.\shim.ps1 install|status|uninstall` instead.

Uninstall the shim:

```bash
./shim.sh uninstall
```

## Build From Source

The workspace is Rust, MSRV 1.96 (`rust-version` in the root `Cargo.toml`). It now holds
`relay-core`, `relay-ipc`, `relay-discovery`, `relay-adapters`, `relay-control`,
`relay-compass`, `relay-cognitive`, `relay-semantic`, `relay-agent`, `relay-shim` and
`relay-gui`. `relay-gui` is not a default member, so a plain `cargo build` skips it.

macOS (needs Rust stable and the Xcode command line tools):

```bash
./build_app.sh
```

The build creates `build/VSCRelay.app` and, with `hdiutil`, `build/VSCRelay.dmg`.
Packaged macOS builds target Apple Silicon; universal Intel support is planned.

Linux:

```bash
./build_linux.sh
```

It builds the daemon and shim, preferring the static `<arch>-unknown-linux-musl` target when
that target and a musl gcc are both installed and falling back to the host `gnu` target, builds
the GUI for the host, and stages `dist/vsc-relay-<version>-linux-<arch>.tar.gz` with the
installer, uninstaller, unit, desktop entry and `relay.env.example`, a `.sha256` beside it, and —
when `dpkg-deb` is present — `dist/vsc-relay_<version>_<debarch>.deb`. Pass a target triple as
the first argument to force one. If the GUI's dev libraries are missing the GUI step is skipped
with a note and the tarball is still built without it.

What the build needs:

- `musl-tools` (and `rustup target add <arch>-unknown-linux-musl`) for the static daemon and
  shim.
- The GUI's development libraries, which on Debian/Ubuntu are the set CI installs:

  ```bash
  sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libgl1-mesa-dev \
    libxcb1-dev libxcursor-dev libxi-dev libxrandr-dev libfontconfig1-dev
  ```

- `dpkg-deb` if you also want the `.deb`.

Release Linux builds run on ubuntu-22.04 so the dynamically linked GUI keeps a low glibc floor.
Note that the local ONNX semantic backend is compiled out on Linux regardless of features, so
`--all-features` here does not give you a `local` backend.

Windows (needs Rust stable and the MSVC toolchain):

```powershell
.\build_windows.ps1
```

It builds the agent, shim, and GUI for `x86_64-pc-windows-msvc` and stages
`dist\vsc-relay-<version>-windows-<arch>.zip` with `install.ps1` and helpers.

To just build the binaries on any platform:

```bash
cargo build --release
```

Add `-p relay-gui` to include the desktop app, which the default members leave out.
