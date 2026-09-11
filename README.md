# VS Code Agent Relay

<p align="center">
  <img alt="Claude Code" src="https://img.shields.io/badge/Claude%20Code-191919?logo=anthropic&logoColor=white">
  <img alt="VS Code" src="https://img.shields.io/badge/VS%20Code-007ACC?logo=visualstudiocode&logoColor=white">
  <img alt="Telegram" src="https://img.shields.io/badge/Telegram-26A5E4?logo=telegram&logoColor=white">
  <img alt="macOS" src="https://img.shields.io/badge/macOS-14%2B-000000?logo=apple&logoColor=white">
  <img alt="Linux" src="https://img.shields.io/badge/Linux-X11-FCC624?logo=linux&logoColor=black">
  <img alt="Windows" src="https://img.shields.io/badge/Windows-10%2F11-0078D6?logo=windows&logoColor=white">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-built-B7410E?logo=rust&logoColor=white">
  <img alt="Latest release" src="https://img.shields.io/github/v/release/itrootvm/vsc_relay?sort=semver">
  <img alt="Downloads" src="https://img.shields.io/github/downloads/itrootvm/vsc_relay/total?label=downloads">
  <img alt="License" src="https://img.shields.io/badge/license-MIT-blue">
</p>

Control long-running Claude Code sessions in VS Code from Telegram.

VSC Relay is for developers who leave Claude Code running in VS Code and do not want work
to stop because a session asked a question, requested permission, or waited for a single
follow-up prompt. It watches your local VS Code agent sessions and gives you a Telegram
control panel for reading status, replying, approving or denying prompts, and sending the
next instruction.

It is not a cloud service. It runs on your Mac, Linux, or Windows machine, talks to Telegram
by outbound HTTPS, and uses local files and local IPC (Unix domain sockets on macOS and
Linux, named pipes on Windows) to observe and control local agent sessions.

![VSC Relay macOS app](docs/screenshot.png)

## Why This Exists

Claude Code is useful for long tasks, but it often needs a human at exactly the wrong time:

- a multiple-choice question appears after you leave the desk;
- a tool or skill permission waits for approval;
- a risky shell command needs a yes or no;
- the task finishes and needs the next instruction;
- several VS Code windows are running and you need to know which one is blocked.

VSC Relay turns that into a Telegram workflow. Start the task on your Mac, Linux, or Windows
box, leave it running, and handle the next decision from your phone.

## What You Get

- A Telegram menu showing open VS Code windows and active Claude Code or Codex chats.
- Notifications when a session finishes, asks a question, errors, or needs attention.
- Background replies into Claude Code chats when the shim is installed.
- Media from Telegram: send or forward a photo, voice message, video, or document and the
  relay stages it in a private local folder, then hands the file path to the session so the
  agent can read it with its own tools. If a chat has no active session, a picker asks where
  to route it. Attach local files to a composed prompt from the desktop app too.
- Telegram buttons for Claude Code questions, including multi-question prompts.
- Allow or Deny controls for permission requests surfaced through the relay.
- Cross review: a model from another family, such as Codex or Antigravity, reads a chat and
  says whether it still serves your last instruction, and can send a correction back into it.
- Session handoff: move a stuck or finished Claude Code session to another agent without
  re-explaining it. See [Session Handoff](#session-handoff) below.
- A decision log of every approval, ask, gate verdict and Telegram delivery outcome, counted
  with `vsc-relay-agent decisions`.
- A blocked-command guard for dangerous shell commands such as `rm -rf`, `drop table`,
  `git push --force`, and patterns you add yourself.
- Claude Code controls for model, reasoning effort, and permission mode.
- Window focus and GUI fallback controls for cases where background control is unavailable.
- A desktop setup app (token, pairing key, live log, service controls, shim install/removal):
  the macOS `VSCRelay.app` plus `vsc-relay-gui` on Linux and Windows. The Linux/Windows app
  also carries a Diagnostics pane that tails the daemon log with a text filter and a Gate
  trace only toggle, session search with All/Active/Attention/Robot filters, a Compass, Steer
  and Gate status strip showing the semantic backend in use, the cross review settings, and
  per-session Cross review, Hand off, Usage, Gate pins and Copy id buttons.

## How The Pieces Fit

```text
Claude Code in VS Code
        |
        | transcript files, hooks, optional shim
        v
vsc-relay-agent on your Mac, Linux, or Windows box
        |
        | Telegram Bot API, outbound HTTPS
        v
Telegram chat on your phone
```

The project is made of three local parts:

- `vsc-relay-agent` discovers sessions, watches transcripts, runs the Telegram bot, handles
  local hook events, and executes control actions.
- `vsc-claude-shim` optionally wraps the Claude Code helper binary so the relay can send
  text and question answers in the background.
- `VSCRelay.app` on macOS, and `vsc-relay-gui` on Linux and Windows, are the desktop UIs for
  setup, starting and stopping the relay, viewing logs, and managing the shim. On Linux the
  relay can equally be supervised headlessly by `systemd --user`.

Reading session state does not use screen scraping. The relay reads the files Claude Code,
Codex, and VS Code already write locally. Elevated desktop access is only needed for window
focus and GUI fallback actions: macOS Accessibility, or an X11 (or XWayland) session with
`xdotool` on Linux, or an interactive desktop session on Windows.

## Support Matrix

| Target | Read status | Send prompt | Answer questions | Permission actions | Model or mode controls |
| --- | --- | --- | --- | --- | --- |
| Claude Code in VS Code | Yes | Yes | Yes, with shim | Yes | Yes, with shim |
| Codex in VS Code | Experimental | GUI fallback only | No | No | No |

Claude Code is the main supported target. Codex support is currently useful for watching
threads, receiving completion or error notifications, and focusing the right VS Code
window. Full background control for Codex is not implemented yet.

## Platform Support

| Capability | macOS 14+ | Linux | Windows 10/11 |
| --- | --- | --- | --- |
| Setup app | `VSCRelay.app` | `vsc-relay-gui` (egui, any desktop) | `vsc-relay-gui.exe` (egui) |
| Packages | `.dmg` | `.deb` or `.tar.gz` | `.zip` + `install.ps1` |
| Supervision | launchd (`./launchd.sh` from a checkout) | `systemd --user` (`vsc-relay.service`) | logon task from `install.ps1` |
| Background control (shim) | Yes | Yes | Yes, over named pipes |
| Window focus / GUI fallback | Yes, via Accessibility | X11 or XWayland + `xdotool`; native Wayland blocks key injection | Yes, via Win32 in an interactive session |
| Automation: Compass, gate, modes | Yes | Yes | Yes |
| Cross review | Yes | Yes | Yes |
| Handoff to an agent CLI | Yes, opens Terminal | Yes, opens an installed terminal emulator | No; there is no Windows terminal launcher |
| Handoff to an editor | Yes, from the installed `.app` bundles | Yes, editors found on `PATH` | Best effort; no Windows-specific launcher |
| Local ONNX semantic backend | Yes | Not built; pick another backend | Yes |

The background shim path (sending prompts, answering questions, permission Allow/Deny, and
model/effort/mode) is the primary control channel and needs no display on any platform. Window
focus and GUI fallback (Codex, un-shimmed sessions) need macOS Accessibility, an X11 or
XWayland session with `xdotool` on Linux, or Windows Win32 in an interactive desktop session.

Compass, the gate, the modes and cross review are daemon features and behave the same
everywhere. What differs is the **semantic backend** Compass uses to read a transcript. The
local on-device ONNX backend ships on macOS and Windows only: the prebuilt ONNX Runtime needs
a recent glibc and has no musl build at all, which would undo "one Linux build runs anywhere",
so Linux builds are compiled without it and their default backend is `off`. Choose one of the
remaining backends there:

```bash
vsc-relay-agent automation smart provider ollama
vsc-relay-agent automation smart provider openai-compatible
vsc-relay-agent automation smart provider claude
vsc-relay-agent automation smart check
```

`ollama` pointed at `localhost` keeps transcript text on the machine. `openai-compatible` and
the agent-CLI providers (`claude`, `codex`, `gemini`, `cursor`, `antigravity`) send transcript
text off the machine, and the command prints that disclosure when you set one.
`automation smart install-local` and `automation smart train-local` prepare that local backend,
so they are macOS and Windows commands. They still run on Linux, but what they produce cannot
be loaded there, and `train-local` additionally switches the configured provider to `local`,
which then fails at inference.

On Linux the daemon and shim are statically linked against musl, so one build runs across
distributions. `vsc-relay-gui` is not: it links the system GL, X11 and Wayland libraries
dynamically, which is why the released GUI is built on an older distribution to keep its glibc
floor low, and why the `.deb` declares library dependencies for it.

## Telegram Commands

The bot also exposes most actions as buttons. `/menu` is the recommended entry point.

| Command | What it does |
| --- | --- |
| `/auth <key>` | Pair the current Telegram chat with the relay. Required before control works. |
| `/menu` | Open the interactive windows and chats menu. |
| `/windows` | List discovered VS Code windows and chats. |
| `/status <workspace>` | Show details for one workspace. |
| `/say <workspace> <claude\|codex> <text>` | Send a prompt to a chat. |
| `/stop <workspace> <claude\|codex>` | Interrupt the current turn. |
| `/cont <workspace> <claude\|codex>` | Send `continue`. |
| `/mode <workspace>` | Cycle Claude Code permission mode. |
| `/slash <workspace> <claude\|codex> <command>` | Send a slash command. |
| `/focus <workspace>` | Raise the matching VS Code window. |
| `/danger` | Show blocked command patterns. |
| `/danger add <pattern>` | Add a blocked command pattern. |
| `/danger del <pattern>` | Remove a blocked command pattern. |
| `/help` | Show the command list. |

The menu buttons add shortcuts for common workflows: choose a window, choose a chat, read
recent messages, send a prompt, continue, stop, focus, change Claude model, change effort,
change permission mode, answer a question, or approve and deny a permission request. Every
chat card also has Cross review, which asks another model to check the chat, and Claude Code
cards add Hand off, which moves the session to another agent.

## Install (macOS)

Use the app if you want the normal experience.

1. Download `VSCRelay.dmg` from
   [Releases](https://github.com/itrootvm/vsc_relay/releases/latest).
2. Open the disk image and drag `VSCRelay.app` to Applications.
3. Start the app. On the first launch, right click and choose Open if macOS blocks it.
4. Open Settings, paste your Telegram bot token from BotFather, and set a pairing key.
5. Click Start.
6. In Telegram, send `/auth <key>` to your bot, then `/menu`.

The app stores the bot token and pairing key in the macOS Keychain. Runtime state lives under
`~/.vsc-relay`, and the daemon's log is the file `~/.vsc-relay/agent.log`, which it rotates
itself. To supervise it with launchd instead of the app, use `./launchd.sh install` from a
checkout.

Current packaged macOS builds target Apple Silicon Macs. A universal build is on the roadmap.

## Install (Linux)

Linux ships a GUI app (`vsc-relay-gui`, built with egui, runs on any desktop environment
like GNOME, KDE, or XFCE, on X11 or Wayland) plus a headless systemd service. All downloads are on
the [Releases](https://github.com/itrootvm/vsc_relay/releases/latest) page.

**Debian / Ubuntu (`.deb`, recommended):**

```bash
sudo apt install ./vsc-relay_<version>_amd64.deb
```

It installs `vsc-relay-agent`, `vsc-claude-shim`, and `vsc-relay-gui` to `/usr/bin`,
`/usr/lib/systemd/user/vsc-relay.service`, a desktop launcher, and a `relay.env.example`, and
pulls the GUI's library dependencies automatically.

**Any other distribution (`.tar.gz`):**

```bash
tar xzf vsc-relay-<version>-linux-<arch>.tar.gz
cd vsc-relay-<version>-linux-<arch>
./install.sh
```

The daemon and shim in the tarball are statically linked against musl, so the same archive
works on any distribution. `install.sh` puts them in `~/.local/bin` and writes the user unit
to `~/.config/systemd/user/vsc-relay.service`. If the bundled `vsc-relay-gui` will not start,
that is the dynamically linked part: install the `.deb` or build the GUI yourself.

Then, on either install:

1. Launch **VS Code Agent Relay** from your app menu (or run `vsc-relay-gui`).
2. Open Settings, paste your Telegram bot token, set a pairing key, click **Install shim**,
   then **Start**.
3. In Telegram, send `/auth <key>` to your bot, then `/menu`.

Prefer headless? Skip the GUI and use the service:

```bash
$EDITOR ~/.config/vsc-relay/relay.env
vsc-relay-agent shim-install
systemctl --user enable --now vsc-relay
systemctl --user status vsc-relay
loginctl enable-linger "$USER"
```

`loginctl enable-linger` is what keeps the user service running after you log out or when the
machine boots without a desktop session; without it systemd stops your user manager and the
relay goes with it.

Run the GUI or the service, not both. The daemon takes a single-instance lock at
`~/.vsc-relay/vsc-relay-agent.lock`, so a second copy logs the process that holds it and
exits, and the GUI's **Start** clears stray daemons before it launches its own.

The daemon's log is a file, `~/.vsc-relay/agent.log`, not journald, so read it with
`tail -f ~/.vsc-relay/agent.log` or the GUI's Diagnostics pane.
`journalctl --user -u vsc-relay` only shows unit start, stop and crash lines.

For window focus and GUI fallback, use an X11 (or XWayland) session and install `xdotool xclip
xdg-utils`; the background shim path works without them.

### Updating (Linux)

The GUI checks the latest release on launch and shows an **Update** banner when a newer
version is out. Click **Update now**, or turn on **Auto-update** in Settings to apply updates
automatically. Headless installs update with:

```bash
vsc-relay-agent self-update          # download + swap binaries + re-wrap the shim
vsc-relay-agent self-update --check  # just report whether a newer release exists
```

Both paths verify the release checksum before swapping the binaries. `.deb` installs can also
update through `apt` when you download a newer `.deb`. Full steps and Wayland notes are in
[docs/installation.md](docs/installation.md).

### Multiple machines

Today each machine runs its own bot: give every machine its own token (name them with
`RELAY_MACHINE_NAME`) and talk to each through its own bot. A single-bot hub that controls
all machines from one chat is on the roadmap.

## Install (Windows)

Windows 10 or 11 (x64). Download `vsc-relay-<version>-windows-x86_64.zip` from
[Releases](https://github.com/itrootvm/vsc_relay/releases/latest), extract it, and run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

It installs `vsc-relay-agent.exe`, `vsc-claude-shim.exe`, and `vsc-relay-gui.exe` to
`%LOCALAPPDATA%\Programs\vsc-relay`, wires the Claude Code hooks, installs the shim, and
registers a logon task that runs the relay in your interactive session. Then:

1. Edit `%APPDATA%\vsc-relay\relay.env` and set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET`.
2. Launch **VS Code Agent Relay** (`vsc-relay-gui.exe`), or start the agent directly:
   `%LOCALAPPDATA%\Programs\vsc-relay\vsc-relay-agent.exe`.
3. In Telegram, send `/auth <key>` to your bot, then `/menu`.

The background shim path (send prompts, answer questions, permission Allow/Deny, and
model/effort/mode) runs over local Windows named pipes and needs no display. Window focus and
GUI fallback need an interactive desktop session (the logon task runs there; a Session-0
service cannot focus windows). Secrets live in `%APPDATA%\vsc-relay\relay.env`, tightened to
your account with `icacls`. Update in place with `vsc-relay-agent.exe self-update`. From a
source checkout the terminal path is `.\svc.ps1 start` and `.\shim.ps1 install`.

## Build From Source

Requirements (all platforms): Rust stable, the Claude Code VS Code extension for full
functionality, and a Telegram bot token from BotFather. On macOS also install the Xcode
command line tools. On Linux install `musl-tools` so the daemon and shim link statically,
`xdotool xclip xdg-utils` for GUI fallback, and, only if you also build `vsc-relay-gui`, the
desktop development libraries:

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libgl1-mesa-dev \
  libxcb1-dev libxcursor-dev libxi-dev libxrandr-dev libfontconfig1-dev
```

Build the macOS app:

```bash
./build_app.sh
```

Build the Linux release tarball and `.deb`:

```bash
./build_linux.sh
```

It produces `dist/vsc-relay-<version>-linux-<arch>.tar.gz` and, where `dpkg-deb` is available,
`dist/vsc-relay_<version>_<amd64|arm64>.deb`. The daemon and shim go to the musl target when
`musl-tools` and the Rust musl target are installed (pass one explicitly with
`./build_linux.sh x86_64-unknown-linux-musl`), otherwise to the host gnu target. The GUI is
always built for the host and is skipped if the desktop libraries above are missing, which is
why release GUI builds run on an older distribution to keep the glibc floor low.

Build the Windows release zip (daemon, shim, and GUI):

```powershell
powershell -ExecutionPolicy Bypass -File .\build_windows.ps1
```

`cargo build --release` builds the daemon and shim (headless, no desktop libs needed). The
GUI is excluded from the default build; build it explicitly with
`cargo build --release -p relay-gui` (needs the desktop dev libs listed above).

Run the relay headless from the terminal:

```bash
cp .env.example .env
# edit .env and set TELEGRAM_BOT_TOKEN and RELAY_PAIR_SECRET
./svc.sh start
```

Service helper commands:

```bash
./svc.sh status
./svc.sh logs
./svc.sh restart
./svc.sh stop
```

To have the OS supervise the clone's `target/release/vsc-relay-agent` instead, each platform
has a helper that writes the unit and manages it. On macOS:

```bash
./launchd.sh install
./launchd.sh status
./launchd.sh uninstall
```

On Linux the same job is `./systemd.sh`, which writes
`~/.config/systemd/user/vsc-relay.service` pointing at the clone, enables it, and wraps the
usual `systemctl --user` verbs:

```bash
./systemd.sh install
./systemd.sh status
./systemd.sh start
./systemd.sh restart
./systemd.sh stop
./systemd.sh logs
./systemd.sh uninstall
```

`./systemd.sh logs` tails `~/.vsc-relay/agent.log`, which is where the daemon actually writes.
`install` stops any running GUI or daemon first, so the new unit owns the single-instance lock.

Install the Claude Code shim from the terminal:

```bash
./shim.sh install
./shim.sh status
./shim.sh uninstall
```

Only Claude Code chats opened after shim installation use the background control channel.
Already-open chats keep using the binary they started with.

## Background Control And The Shim

Claude Code talks to a helper binary from the VS Code extension. Normally, the relay can
read transcript files and use GUI fallback, but it cannot write directly into that helper.

The shim solves that. It wraps the helper, forwards normal stdin and stdout unchanged, and
opens local sockets that let the relay inject a user message or answer a Claude Code
question. If the shim cannot initialize, it falls back to the real helper so the chat still
starts.

The shim modifies the Claude Code extension's native helper in place:

- the original helper is moved to `claude.real`;
- `vsc-claude-shim` is copied as `claude`;
- uninstalling restores `claude.real`.

The installer refuses to replace a file that does not look like the real Claude Code helper.
When the Claude Code extension updates, new chats may need the shim installed again.

## Session Handoff

Handoff moves a Claude Code session to a different agent without you retelling the story. The
relay writes a brief, `HANDOFF.md`, at the root of the workspace the session was already
working in (any earlier brief there is rotated to `HANDOFF.prev.md`), then delivers a prompt
that points the new agent at it. The work stays where it is; only the agent changes.

Three kinds of destination are offered, and delivery follows the kind:

- **Another Claude chat** the relay can see: the prompt goes straight in through the shim.
- **An agent CLI** installed on the machine (`claude`, `codex`, `agy`, `cursor-agent`): the
  relay starts it in a terminal already sitting in the project, with the prompt as its first
  message. Where the CLI can report it — `codex`, `cursor-agent`, `agy` — the relay checks
  sign-in first, so a handoff that would stall on a login screen says so in the same message
  that started it. Claude Code has no headless equivalent, so nothing is claimed about it.
- **An editor** installed on the machine (Antigravity, Cursor, Windsurf, VSCodium, VS Code):
  the relay opens it on the same project and you paste the prompt into its own chat.

The receiving agent is asked to append a receipt to the brief before it changes any code, and
`handoff receipt` reads that block back and reports which contract anchors it covered.

On Linux both destination kinds work. Editors are located on `PATH` (`code`, `code-insiders`,
`code-oss`, `codium`, `vscodium`, `cursor`, `windsurf`, `antigravity`), and a CLI handoff opens
a real terminal window, trying `x-terminal-emulator`, `ptyxis`, `kgx`, `gnome-terminal`,
`konsole`, `xfce4-terminal`, `mate-terminal`, `tilix`, `terminator`, `alacritty`, `wezterm`,
`kitty`, `foot`, `lxterminal`, `deepin-terminal`, `qterminal`, `urxvt`, `st` and `xterm` in
that order. With no graphical session the handoff fails with a message naming the generated
launcher script, so you can run it yourself in any shell. Windows has no launcher of its own
yet, so CLI handoff there is not supported; the brief and the receipt still work.

Reach it from the **Hand off** button on a Claude Code chat card in Telegram, from the macOS
app, from the Hand off window in `vsc-relay-gui` (grouped as Claude chats, Agent CLIs and
Editors), or from the terminal:

```bash
vsc-relay-agent handoff destinations SESSION_ID
vsc-relay-agent handoff SESSION_ID --to DESTINATION_ID
vsc-relay-agent handoff receipt /path/to/workspace
```

More detail, including how the brief is built, is in
[docs/architecture.md](docs/architecture.md).

## Security Model

- Telegram users cannot control anything until their chat is paired with `/auth <key>` or
  added through `TELEGRAM_ALLOWED_CHATS`.
- The relay does not open an inbound network port. Telegram communication is outbound
  HTTPS long polling.
- Runtime IPC is local only: Unix domain sockets in an owner-only `0700` directory under
  your account on macOS and Linux, and per-user-DACL named pipes
  (PIPE_REJECT_REMOTE_CLIENTS) on Windows. No inbound network port is opened.
- Secrets live with the platform: the macOS app stores them in Keychain, `vsc-relay-gui` on
  Linux writes them to `~/.config/vsc-relay/relay.env` with `0600` permissions, and on Windows
  they live in `%APPDATA%\vsc-relay\relay.env`, tightened to your account with `icacls`. The
  terminal mode reads them from `.env`; keep that file private and out of version control.
- Dangerous command patterns are checked before execution through Claude Code hooks.

This tool can type into your local agents and approve or deny their actions. Use a strong
pairing key and review your blocked-command list.

## Documentation

- [Installation](docs/installation.md)
- [Operating guide](docs/operating.md)
- [Architecture](docs/architecture.md)
- [Security details](SECURITY.md)
- [Threat model](docs/threat-model.md)
- [Troubleshooting](docs/troubleshooting.md)
- [FAQ](docs/faq.md)
- [Codex support](docs/codex-support.md)
- [Contributing](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)

## Roadmap

- Universal macOS build for Intel and Apple Silicon.
- More complete Codex background control.
- More robust callback payloads for workspaces with unusual names.
- Native Wayland input support for the GUI fallback (the shim path already works on Wayland).
- Multiple machines reporting to one Telegram bot.

## Repository Layout

```text
crates/
  relay-core        shared types and state reduction
  relay-ipc         local IPC transport (Unix domain sockets on macOS and Linux, named pipes on Windows)
  relay-discovery   VS Code, Claude Code, Codex, and git discovery
  relay-adapters    transcript readers for Claude Code and Codex
  relay-control     window focus and GUI fallback actions (macOS, Linux, and Windows backends)
  relay-compass     goal, contract, steps, gate and handoff model behind the smart layer
  relay-cognitive   expectation and observation records used to verify a session
  relay-semantic    semantic backends (local ONNX on macOS/Windows, ollama, openai-compatible, agent CLI)
  relay-agent       daemon, Telegram bot, hooks, auth, ingress, control
  relay-shim        Claude Code helper wrapper
  relay-gui         Linux and Windows desktop app (egui), any desktop environment
macapp/             SwiftUI macOS app
packaging/linux/    systemd user service, desktop launcher, install/uninstall/deb scripts
packaging/windows/  install/uninstall PowerShell scripts and relay.env.example
build_app.sh        macOS app bundle and disk image builder
build_linux.sh      Linux tarball + .deb builder (static daemon + shim, dynamic gui)
build_windows.ps1   Windows release zip builder (daemon + shim + gui)
launchd.sh          macOS launchd user agent helper (install|uninstall|status)
systemd.sh          Linux systemd --user helper for a dev clone (install|uninstall|status|start|stop|restart|logs)
svc.sh              terminal service helper
svc.ps1             terminal service helper (Windows)
shim.sh             terminal shim helper
shim.ps1            terminal shim helper (Windows)
```

## Development

Before sending a change:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo audit
```

Do not commit `.env`, tokens, local logs, `target/`, or `build/`.

## Maintainer

Maintained by [itrootvm](https://github.com/itrootvm).

Quick contact: [@chossi](https://t.me/chossi).
Bugs and feature requests: use GitHub Issues.
Security reports: see [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
