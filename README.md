# VS Code Agent Relay

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-macOS%2014%2B-lightgrey.svg)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)
![Latest release](https://img.shields.io/github/v/release/itrootvm/vsc_parser?sort=semver)

Watch and control your Claude Code chats inside VS Code from Telegram.

You start a long task in a coding agent, then you leave the desk. The agent keeps
working, and at some point it stops and waits: it finished a turn, it hit an error, it
wants to run a command it is not sure about, or it is asking you a multiple-choice
question. Normally that means the work is parked until you are back at the keyboard. This
tool closes that gap. From a Telegram chat on your phone you can see what each session is
doing, read the last messages, send a new instruction, answer the question it asked,
approve or block a risky command, switch the model or reasoning effort, and change the
permission mode. You do this without walking back to the machine and without it flipping
windows around on screen, because the control happens in the background.

It runs as a small local service on your Mac, talks to your Telegram bot, and does its
reading by tailing the files the agent already writes to disk. It is written in Rust,
ships as a self-contained macOS app, and is built for macOS first.

## What it does

- Finds every open VS Code window and the agent session running in it, by reading the
  files the tools already write to disk. No screen scraping for reading.
- Tails the live transcript of each chat and pushes updates to Telegram: a session
  started, a turn finished, an error, a question, a pending permission.
- Lets you send a message into a chat from Telegram and have the agent act on it, in the
  background, so you can keep a task moving while you are away from the machine.
- Answers Claude Code questions (the multiple-choice `AskUserQuestion` prompts, including
  the multi-question and multi-select ones) from Telegram, in the background.
- Answers any tool or skill permission prompt (the "Do you want to proceed" and "Use skill"
  dialogs, for example Artifact, Skill, or a sensitive command) from Telegram with Allow or
  Deny.
- Switches the Claude Code model, reasoning effort, and permission mode from Telegram.
- Intercepts destructive shell commands (for example `rm -rf`, `drop table`,
  `git push --force`) before they run and asks you to allow or block them. The blocked
  list is editable from Telegram.
- Sends only a short preview in a notification and keeps a "Show full text" button so a
  long message never gets lost to truncation.
- Raises a specific window on the Mac when you do want to look at it.
- Shows a small dashboard in the app: whether the service is running, a live log, the
  installed Claude Code version, and a count of sessions and completed turns for the day.

## Claude Code and Codex

Claude Code is the supported target. Everything above works with it: reading, sending,
answering questions, switching model and effort and mode, and the destructive-command
guard through the hook and the shim.

Codex support is experimental and, for now, read plus window focus. The relay discovers
Codex threads and tails their transcripts, so you can watch them and get turn-complete and
error notifications, and it can raise a Codex window on screen. Interactive background
control for Codex (sending and answering in the background the way Claude Code does) is in
development. Because Codex threads on this setup usually run with approvals disabled, they
rarely block waiting for input, so the read side covers most of what you need today. Treat
Codex as opt-in and expect the interactive parts to land later. See the Roadmap.

## Requirements

To run the packaged app you only need:

- macOS 14 or later on Apple Silicon or Intel. Developed and tested on macOS 15 (Sequoia).
- The Claude Code VS Code extension, 2.1.x. Codex support expects the OpenAI Codex
  app and the ChatGPT VS Code extension.
- A Telegram bot token from @BotFather.

You do not need Rust or Xcode to run the app. Those are only needed to build it from
source (see Building from source).

## How it works

The core is one background process, the relay daemon, plus a small wrapper that gives
it a clean control channel into Claude Code.

- The relay daemon (`vsc-relay-agent`) does all the local work. It discovers windows,
  tails transcripts, runs the Telegram bot over outbound long polling, listens on a
  local Unix socket for hook events, and carries out commands you send.
- The Claude Code hook (`vsc-relay-agent hook ...`) is a short-lived process that the
  Claude Code extension runs on tool use and other events. It forwards the event to the
  daemon over the local socket, and for a pending command it can return an allow or block
  decision. This is how destructive commands are stopped before they run.
- The shim (`vsc-claude-shim`) is an optional wrapper around the Claude Code helper
  binary. When installed it lets the daemon write into a chat and answer questions in the
  background. See Background control below.

State lives under `~/.vsc-relay`: the local sockets, the list of authorized Telegram
chats, the blocked-command list, the log, and the pid file. The directory is created with
owner-only permissions and the state files are written the same way. The app keeps your
bot token and pairing key in the macOS Keychain, not in a file. The terminal path reads
them from a `.env` file in the project folder instead.

## Install

There are two ways to install: download the ready app, or clone the source and build it
yourself. Both need a Telegram bot token. Open Telegram, talk to @BotFather, send
`/newbot`, and copy the token it gives you.

### Path 1: the app (no dependencies, nothing to compile)

This is for everyone. You do not need Rust, Xcode, or a terminal. The app already contains
the compiled binaries inside it, so it runs on a stock Mac with nothing else installed.

1. Download `VSCRelay.dmg` from the
   [Releases page](https://github.com/itrootvm/vsc_parser/releases/latest), open it, and drag
   the app onto the Applications folder shown in the window. `INSTALL.txt` in the same window
   repeats these steps.
2. Open it from Applications. The first time, right click the app and choose Open, so
   macOS allows an app from outside the App Store.
3. Click Settings, paste your bot token, and set a pairing key. The key is required; use
   Generate if you want a strong random one. Both are stored in your Keychain.
4. Click Start, then in Telegram send `/auth <key>` to your bot and `/menu`.

Once a token and key are set, the app starts the service by itself on launch, so in normal
use you do not click Start. Settings has a Launch at login toggle so the app comes up with
your Mac and the service is running without you touching anything.

The window shows whether the service is running, a small dashboard, Start and Stop, a live
log, Install shim and Remove shim for background control, and a button to open the
Accessibility pane. There is also a menu-bar item near the clock with the same status,
Start and Stop, the recent log, and a way to reopen the window. The service keeps running
while the app is open. Closing the window leaves it running; use Quit (Command Q) to stop
it. The log at `~/.vsc-relay/agent.log` rotates automatically and will not grow without
bound. If it ever finds two copies of the service running it stops the extra one, so they
do not fight over Telegram.

### Path 2: clone the source

This is for building the app yourself or running it headless. You need Rust
(https://rustup.rs) and the Xcode command line tools.

To build the same app the other path uses:

```
./build_app.sh
```

That produces `build/VSCRelay.app` and `build/VSCRelay.dmg`. The result is self contained
and can be copied to another Mac that has no Rust installed.

To run it headless from a terminal instead, without the app window:

```
cp .env.example .env
# edit .env and set TELEGRAM_BOT_TOKEN and RELAY_PAIR_SECRET
./svc.sh start
./shim.sh install    # optional, enables background control
```

`./svc.sh` also accepts `stop`, `restart`, `status`, and `logs`. Run either the app or
`svc.sh`, not both at once, since two copies would both poll Telegram. If you click Start
in the app while a terminal copy is running, the app stops the old one first, so they do
not conflict.

## Using it

In Telegram, send `/auth <secret>` once to authorize your chat. After that:

- `/menu` shows your windows and chats as buttons.
- `/status <workspace>` shows details for one workspace.
- `/say <workspace> <claude|codex> <text>` sends a prompt.
- `/stop`, `/cont`, `/mode`, `/slash`, `/focus` control a chat.
- `/danger` manages the blocked-command list.

The command list also appears in Telegram's built-in menu. The daemon registers it with
Telegram on startup, so it is tied to the bot token. If you deploy on another machine
with the same token you do not register anything by hand; the menu is already there.

## Background control (the shim)

The Claude Code extension talks to a bundled helper binary over its standard input and
output. Normally nothing else can write to that channel. The shim installs a small
wrapper in front of the real helper. The real helper is renamed to `claude.real` and the
wrapper takes its place. The wrapper starts the real helper, passes everything through
unchanged, and also opens two local sockets so the daemon can inject a message or a
question answer and observe the output. If the wrapper cannot set itself up for any
reason it falls back to running the real helper directly, so a chat never breaks because
of it.

Install and remove it from the app, or with `./shim.sh install` and `./shim.sh uninstall`.

Only chats you open after the shim is in place are controllable in the background. A chat
that is already open keeps running on whatever binary it started with, so installing or
reinstalling the shim does not affect it until you open a new chat. This is worth
remembering: if background control is not working for a chat, the usual reason is that the
chat predates the current shim.

The Claude Code extension updates itself into a new versioned folder, which drops in a
fresh, unwrapped helper, so background control would stop for new chats until the shim is
put back. The service watches for this. It checks the installed version continuously and,
when it sees the shim is missing on a new version, reinstalls it and sends you a Telegram
message saying the update landed, the shim was reinstalled, and that only new chats will
use it. It also tells you in Telegram when a newer Claude Code version is available on the
Marketplace. This is fail safe: the wrapper refuses to install unless the file it is
replacing is the real 229MB helper, and if it ever cannot set itself up it runs the real
helper directly, so a chat never breaks. If the reinstall fails for any reason you get a
Telegram warning and existing chats are unaffected. If you run from the terminal instead of
the app, re-run `./shim.sh install` after an extension update.

## Permissions it asks for

- Network: outbound HTTPS to api.telegram.org only. The daemon does not open any inbound
  network port.
- Accessibility (macOS): only for the window focus and type fallback. Background control
  through the shim does not need it. If you skip it, focusing and typing on screen will
  not work, but everything else does.
- No Screen Recording is required.

## Security

The design assumes the Telegram side is the untrusted edge and keeps the sensitive
material off it.

- No chat can do anything until it pairs. Sending `/auth <secret>` with the correct
  secret authorizes that chat and nothing else does. The secret is compared by hashing
  both sides and comparing the digests, so the comparison does not short circuit and does
  not leak the length, and repeated wrong attempts from a chat are locked out for a
  cooldown period, so the secret cannot be brute forced quickly.
- The app keeps the bot token and pairing key in the macOS Keychain, not in a plaintext
  file. The terminal path keeps them in `.env`, which is created with owner-only
  permissions and is excluded from version control. Neither value is ever sent to a
  Telegram user or printed in a chat.
- The daemon never listens on the network. It reaches Telegram by dialing out and long
  polling, so there is no inbound port for anyone to reach.
- The state directory `~/.vsc-relay` is created with owner-only permissions (0700) and the
  state files with 0600, so another user account on the machine cannot read them.
- Destructive shell commands are intercepted by the Claude Code hook and held for your
  decision before they run. The blocked list ships with sane defaults and you can edit it
  from Telegram.

What someone who finds your bot cannot do: control any chat without the pairing key, read
the bot token (it is never transmitted), or reach the daemon over the network.

Use a long random pairing key. Treat it like a password. Anyone who has both your bot and
the key can drive your editors, so set a strong key and do not share it.

## A word of caution

This tool can type into your live coding agents and approve or block the commands they
try to run. That is the point of it, and it is also why you should set it up carefully.
Use a strong pairing key, keep it private, and review the blocked-command list so it
matches what you consider dangerous. The shim modifies a file inside the Claude Code
extension; removing the shim restores the original.

## Uninstall

If you used the app: Quit it, click Remove shim first if you installed it, then delete the
app from Applications. To clear its stored secrets, remove the `dev.vscrelay.app` items
from Keychain Access.

If you used the terminal path:

```
./svc.sh stop
./shim.sh uninstall
rm -rf ~/.vsc-relay
```

## Roadmap

- Interactive background control for Codex (send and answer in the background), to match
  what Claude Code already supports.
- A universal build so the app also runs on Intel Macs. The current build targets Apple
  Silicon.
- More reliable detection of a Claude Code session that is stuck waiting on a permission
  prompt, so it always shows up as an actionable card rather than looking busy.
- Windows and Linux agents. The core, the transport, and the adapters are kept separate
  from the macOS-specific control layer so other platforms can slot in later.
- Multiple machines reporting into one Telegram bot.

## Contributing

Issues and pull requests are welcome. A few ground rules that keep the tree consistent:

- `cargo fmt --all` and `cargo clippy --all-targets --all-features -- -D warnings` must be
  clean, and `cargo test` must pass.
- Source is English only, including error messages and test data. No Russian.
- No comments in source files.
- Do not commit secrets. `.env` and anything with a real token stay out of version
  control.

Build the app with `./build_app.sh` (needs Rust and the Xcode command line tools), or run
the daemon directly with `./svc.sh start` for faster iteration.

## Layout

```
crates/
  relay-core        shared types, state engine
  relay-discovery   finds windows and sessions from disk
  relay-adapters    reads Claude Code and Codex transcripts
  relay-control     macOS window focus and typing
  relay-agent       the daemon binary, Telegram bot, hooks, control
  relay-shim        the Claude Code helper wrapper
macapp/             the SwiftUI app that wraps the daemon with a window
build_app.sh        builds and packages VSCRelay.app and the disk image
```

## License

MIT. See LICENSE.
