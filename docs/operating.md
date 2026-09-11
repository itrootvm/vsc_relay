# Operating the relay

This is the day to day guide: how to get the relay running, how to keep it running, and how
to find out what it did. For first time setup of the bot itself, see `installation.md`.

The relay runs the same daemon on macOS, Linux, and Windows, but supervision, paths, and one
feature differ. Every section below names the platform before the command, so read the block
with your platform's name on it and ignore the others.

## Install

**macOS.**

1. Open `VSCRelay.dmg` and drag `VSCRelay.app` to Applications.
2. Launch it. On the first launch, right click the app and choose Open, so macOS allows an
   app that did not come from the App Store.
3. Open Settings, paste the Telegram bot token from BotFather, set a pairing key, click Start.
4. In Telegram, send `/auth <your key>` to the bot, then `/menu`.

The token and pairing key are stored in Keychain. A rebuilt app is a different signature, so
macOS asks for Keychain access again after an update. Choose Always Allow, otherwise the
daemon starts without a token and Telegram stays silent.

**Linux.** On Debian or Ubuntu, install the `.deb`, which pulls the GUI's library
dependencies through apt:

```bash
sudo apt install ./vsc-relay_<version>_amd64.deb
```

It puts `vsc-relay-agent`, `vsc-claude-shim`, and `vsc-relay-gui` in `/usr/bin`, the service
unit in `/usr/lib/systemd/user/vsc-relay.service`, a desktop entry named **VS Code Agent
Relay**, and a template at `/usr/share/doc/vsc-relay/relay.env.example`.

On any other distribution, unpack `vsc-relay-<version>-linux-<arch>.tar.gz` and run its
installer. The daemon and shim are static musl binaries, so they do not care which
distribution you are on:

```bash
./install.sh
```

That writes the binaries to `~/.local/bin`, the unit to
`~/.config/systemd/user/vsc-relay.service`, the environment file to
`~/.config/vsc-relay/relay.env`, a desktop entry, and wires the Claude Code hooks.

Either way, launch **VS Code Agent Relay** from the app menu, put the bot token and pairing
key in Settings, and click Start. Start also wires the Claude Code hooks; **Install shim**
adds background control. Then send `/auth <your key>` and `/menu` in Telegram.

For a headless box there is no GUI step. The `.deb` does not create the environment file, so
copy the template once, then fill it in:

```bash
mkdir -p ~/.config/vsc-relay
cp /usr/share/doc/vsc-relay/relay.env.example ~/.config/vsc-relay/relay.env
chmod 0600 ~/.config/vsc-relay/relay.env
$EDITOR ~/.config/vsc-relay/relay.env
vsc-relay-agent install-hooks
vsc-relay-agent shim-install
```

Set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in that file. The tarball installer already
creates it and runs `install-hooks` for you, so there you only edit it and run `shim-install`.

Nothing on Linux is stored in a keyring. `relay.env` is the token store, which is why the
tarball installer creates `~/.config/vsc-relay` mode 0700 and the file mode 0600, and why the
`chmod` above matters after a `.deb` install.

**Windows.** Unpack the release zip and run `.\install.ps1` from PowerShell. It copies the
three executables to `%LOCALAPPDATA%\Programs\vsc-relay`, writes `%APPDATA%\vsc-relay\relay.env`,
wires the hooks and the shim, and registers the `VSCRelay` logon task. Put the token and
pairing key in `relay.env`, then start `vsc-relay-gui.exe` and click Start.

## Where the binary lives

Later sections call `vsc-relay-agent` directly. If it is not on your `PATH`, use the full
path for your install:

| Install | Path |
| --- | --- |
| macOS app | `/Applications/VSCRelay.app/Contents/Resources/vsc-relay-agent` |
| Linux `.deb` | `/usr/bin/vsc-relay-agent` |
| Linux tarball | `~/.local/bin/vsc-relay-agent` |
| Windows zip | `%LOCALAPPDATA%\Programs\vsc-relay\vsc-relay-agent.exe` |
| Any clone | `./target/release/vsc-relay-agent` |

## Keeping it running

Every platform offers two ways to supervise the daemon — the app on its own, or the platform's
own supervisor — and only one of them should own it at a time.

**The app alone.** Start and Stop in the window. Good enough for a laptop that gets opened
and closed. Nothing restarts the daemon if the machine reboots.

**launchd (macOS).** Survives logout and reboot, and restarts the daemon if it crashes. From
a clone of the repo:

```bash
./launchd.sh install
./launchd.sh status
./launchd.sh uninstall
```

With the job installed, the app no longer starts a copy of its own. It asks launchd to start
the daemon and attaches to it, which the header shows as "Running (pid N)". Stop in the app
takes the launchd job down, so it stays stopped until you press Start again.

**systemd --user (Linux).** The Linux twin of the launchd job. Both installers ship the unit,
so there is nothing to generate: the `.deb` puts it in `/usr/lib/systemd/user/vsc-relay.service`
and the tarball installer writes `~/.config/systemd/user/vsc-relay.service`. Either way:

```bash
systemctl --user enable --now vsc-relay
systemctl --user status vsc-relay
systemctl --user restart vsc-relay
systemctl --user disable --now vsc-relay
```

A user service stops when your last session ends. To keep the relay up across logout and
reboot:

```bash
loginctl enable-linger "$USER"
```

The unit reads `~/.config/vsc-relay/relay.env` and restarts the daemon on failure.

From a clone, `./systemd.sh` is what `./launchd.sh` is on macOS. It writes
`~/.config/systemd/user/vsc-relay.service` pointing at that clone's own
`target/release/vsc-relay-agent`, so the service supervises exactly what you just built:

```bash
cargo build --release -p relay-agent
./systemd.sh install
./systemd.sh status
./systemd.sh logs
./systemd.sh uninstall
```

`install` refuses to run before the binary exists, creates `relay.env` if it is missing, stops a
running GUI or agent so the lock is free, then enables and starts the unit. `start`, `stop` and
`restart` drive it afterwards, and `logs` tails `~/.vsc-relay/agent.log`, not the journal.

To install a clone the way a release would instead — binaries in `~/.local/bin`, unit written
against that path, hooks wired — run the packaging installer, which picks the binaries out of
`target/release`:

```bash
cargo build --release -p relay-agent -p relay-shim
./packaging/linux/install.sh
```

Add `cargo build --release -p relay-gui` if you also want the desktop app from that clone; it
needs the system GL and X11 development libraries, and the installer simply skips it when it is
not there.

Unlike macOS, the Linux GUI does not attach to the service. Its Start button spawns a daemon
of its own, and before it does it terminates any stray `vsc-relay-agent` it finds, including
the one the service owns. Run the GUI or the service, never both.

**The logon task (Windows).** `.\install.ps1` registers `VSCRelay`, which starts the agent in
your interactive session at logon. `.\uninstall.ps1` removes it.

Only one daemon may run per machine. A second copy exits immediately and writes the pid of
the one holding the lock into `agent.log`. This is deliberate: two daemons polling the same
bot fight over every Telegram update, and Telegram answers the loser with a 409.

To confirm there is exactly one, compare the supervisor with the lock file.

On macOS:

```bash
./launchd.sh status
cat ~/.vsc-relay/vsc-relay-agent.lock
```

On Linux:

```bash
systemctl --user status vsc-relay
cat ~/.vsc-relay/vsc-relay-agent.lock
```

Both should name the same pid. On Windows there is no lock file — the single instance is a
named mutex — so compare `.\svc.ps1 status` against the pid the GUI header shows.

## What is on disk

Everything lives in `~/.vsc-relay` (`%USERPROFILE%\.vsc-relay` on Windows):

| File | What it is |
| --- | --- |
| `agent.log`, `agent.log.1` ... `.12` | The running narrative. The daemon writes and rotates it itself, so rotation works whoever started it. |
| `decisions.jsonl`, `.1` ... `.16` | One line per decision the relay made. About two weeks at normal volume. |
| `automation.json` | Modes, per chat pins, the Auto and Robot rules, cross review settings, and the semantic backend. |
| `robot-review-state.json` | When each chat was last cross reviewed. Session ids are stored hashed. |
| `compass/` | Per session dossiers, the contract ledger, and the dossier digest key. |
| `provider-keys.json` | API keys for the review and semantic providers, written owner-only. |
| `models/semantic/` | The local semantic bundle, when one is installed. macOS and Windows only. |
| `danger.txt` | Optional. If this file exists and has content, it **replaces** the built in danger patterns rather than adding to them. |
| `vsc-relay-agent.lock` | Holds the pid of the daemon that owns this machine. macOS and Linux. |

The bot token and pairing key are the exception. On macOS they are in Keychain; on Linux they
are in `~/.config/vsc-relay/relay.env`; on Windows in `%APPDATA%\vsc-relay\relay.env`.

## Modes

Set the default with `/auto` in Telegram, or pin one chat from its menu.

**Manual.** Every permission request reaches you. Nothing happens without an answer.

**Auto.** Routine approvals go through without asking. Commands matching the danger list are
forwarded to Telegram and wait up to 110 seconds; with no answer the relay falls back to the
prompt in VS Code, so work is never silently approved.

**Robot.** Dangerous actions are denied outright instead of being forwarded, so the agent has
to replan without them. Robot never asks you a question mid turn.

## Finding out what happened

The decision log answers "what did the relay decide, and why", without reading prose:

```bash
vsc-relay-agent decisions --since 24h --by outcome
vsc-relay-agent decisions --since 7d  --by tool
vsc-relay-agent decisions --since 24h --by alias
vsc-relay-agent decisions --since 24h --by reason
vsc-relay-agent decisions --since 24h --by danger_pattern
```

`--since` takes a value like `30m`, `24h` or `7d`. `--by` takes any field present in the
records. It reads `~/.vsc-relay/decisions.jsonl` and its rotations, so it works whether the
daemon is running or not. Any copy of the binary reads the same store, so use whichever path
your install put it at.

On macOS:

```bash
/Applications/VSCRelay.app/Contents/Resources/vsc-relay-agent decisions --since 24h --by outcome
```

On Linux, from the `.deb` or from the tarball:

```bash
/usr/bin/vsc-relay-agent decisions --since 24h --by outcome
~/.local/bin/vsc-relay-agent decisions --since 24h --by outcome
```

## Reading the log

The daemon's own log is a file. It is not stdout, and it is not the journal.

**macOS.**

```bash
tail -f ~/.vsc-relay/agent.log
```

The launchd job additionally captures anything the process prints before tracing starts in
`~/.vsc-relay/agent-stdout.log`, which is usually empty and only interesting after a crash on
startup.

**Linux.** The daemon installs its tracing subscriber against `~/.vsc-relay/agent.log` and
writes nothing to stdout, so `journalctl --user -u vsc-relay -f` shows the unit starting,
stopping, and exiting, and almost nothing about what the relay actually did. Do not use it to
read the relay's log. Use the file:

```bash
tail -f ~/.vsc-relay/agent.log
```

The journal answers exactly one question — did the unit start, and why did it exit — and that
is the only thing to use it for:

```bash
journalctl --user -u vsc-relay -n 50
```

The GUI tails the same file in its Diagnostics pane, with a text filter, a **Gate trace only**
toggle, and a **This session only** toggle once a session is selected. That pane is the fastest
way to watch one workspace without writing a `grep`.

**Windows.** `%USERPROFILE%\.vsc-relay\agent.log`, and `.\svc.ps1 logs` tails it.

Every five minutes the daemon writes one summary line per tool showing how much of the
traffic the gate could actually read:

```bash
grep 'stage="census"' ~/.vsc-relay/agent.log | tail -5
```

## The semantic backend

This is the one feature that genuinely differs by platform.

The Compass smart layer can classify what a session is doing with a local ONNX model that
never leaves the machine. That backend exists on **macOS and Windows only**. It is not built
on Linux: the prebuilt ONNX Runtime needs a glibc newer than most distributions ship and has
no musl build at all, which would cost the Linux release its "runs anywhere" property. On a
Linux build the default backend is therefore `off`. Setting the provider to `local` there is
still accepted and saved, but every use of it then fails with a message saying this build has
no local ONNX backend; `automation smart check` is the quickest way to see that.

Pick a backend:

```bash
vsc-relay-agent automation smart provider off
vsc-relay-agent automation smart provider ollama
vsc-relay-agent automation smart provider openai-compatible
vsc-relay-agent automation smart provider claude
vsc-relay-agent automation smart check
```

The accepted providers are `off`, `local`, `ollama`, `openrouter`, `nvidia`,
`openai-compatible`, and the agent CLIs `claude`, `codex`, `gemini`, `cursor`, `antigravity`.
On Linux, everything except `local` works. `ollama` points at `http://localhost:11434` and
stays on the machine; the rest send transcript text to whatever you point them at, and the
command prints that disclosure when you pick one.

Name the model, endpoint, and key with:

```bash
vsc-relay-agent automation smart model <model-id>
vsc-relay-agent automation smart endpoint https://host/v1
vsc-relay-agent automation smart key -
vsc-relay-agent automation smart status
```

`key -` reads the secret from stdin so it never lands in shell history; `key clear` removes it.

`automation smart install-local` downloads the local bundle and `automation smart train-local`
retrains it; both are macOS and Windows only in effect, because no Linux build can load the
result. The GUI's status strip shows Compass, Steer, and Gate along with the active semantic
backend, so you can see at a glance which one a machine ended up on.

## Cross review

Cross review asks a model from a different family to read a chat and say whether the work still
serves your last instruction. Claude chats are never graded by Claude, and Codex chats are never
graded by Codex.

Turn it on and shape it from `/auto` in Telegram, from Settings in the app, or from a terminal:

```bash
vsc-relay-agent automation review on
vsc-relay-agent automation review reviewers codex-cli,antigravity,claude-cli
vsc-relay-agent automation review every 900
vsc-relay-agent automation review depth normal
vsc-relay-agent automation review budget 6
```

`reviewers` is the order the relay asks them in. `every` is the gap in seconds between scheduled
reviews of one chat, and `budget` caps how many one chat gets. `depth` is how much of the chat
the reviewer reads: `shallow` is the last 8 messages, `normal` 20 and `deep` 50, always together
with the latest real instruction you gave, even when that is older than the window.

In Auto a scheduled review is only recorded and posted to Telegram. A correction reaches the chat
on its own only in Robot, and only after `automation review steer on`. Every correction first
passes the same guard as auto-steer, which refuses anything asking for sudo, force pushes,
sandbox escapes or deleting things.

To review one chat right now, open its card in Telegram and press Cross review. Pick a reviewer,
or the default order, and optionally a depth. The verdict arrives as a separate message within
about a minute, and a correction, if there is one, goes straight into the chat. Pressing the
button is the consent, so this works in any mode, for Claude and Codex chats alike. The session
card in the app has the same menu, on Linux and Windows next to **Hand off**, **Usage**,
**Gate pins** and **Copy id**, and so does the terminal:

```bash
vsc-relay-agent automation review run <session-or-thread-id> --reviewer antigravity --depth deep
```

The reviewer grades your latest instruction, not a short follow-up. A question such as when the
work will be ready is shown to it as context, and the task it grades is the request before it.

## Telegram delivery

Where Telegram is blocked or unreliable, give the relay a proxy. It is a local setting: put it in
`~/.config/vsc-relay/relay.env` on the machine that runs the relay, never in the repository. On
Windows that file is `%APPDATA%\vsc-relay\relay.env`.

```
VSC_RELAY_PROXY=socks5://user:password@proxy.example:1080
```

With a proxy configured the relay starts on it, checks every fifteen minutes whether the direct
route answers, and moves back when it does. A message that fails before reaching Telegram is
tried up to three times, one and then three seconds apart, and since two failures in a row
switch the route, the last try goes the other way. A message that failed after leaving the
machine is not repeated, because Telegram may already have delivered it.

To see how delivery went:

```bash
vsc-relay-agent decisions --since 24h --by outcome | grep send_
```

`send_recovered` is a message a retry saved. `send_lost` is one no route could deliver, which
almost always means the direct route and the proxy were down at the same time, for example a
proxy that is only reachable while a VPN is connected.

On Linux the environment file is read by the systemd unit, so edits to it need a
`systemctl --user restart vsc-relay` before they take effect.

## Tuning the danger list

Danger matching is a plain substring test, which is easy to get wrong in both directions. Check
a candidate list against real commands before trusting it. `danger-check` reads one command per
line and names the pattern that fires:

```bash
vsc-relay-agent danger-check <<'EOF'
rm -rf build
psql -c '\pset format unaligned' -f query.sql
kubectl delete pod postgres-0
EOF
```

Judge patterns on real traffic, never on intuition. A rule that fires on routine work teaches
people to approve without reading, which is worse than having no rule.

## Terminal only

No app, no window. From a clone, on macOS and Linux:

```bash
cp .env.example .env
./svc.sh start
./svc.sh status
./svc.sh logs
```

On Windows, `.\svc.ps1 start|stop|restart|status|logs` drives the same daemon.

Put `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in `.env` first. Do not run this alongside the
app, the launchd job, or the systemd service; the lock will simply refuse the second one.

## Background control for Claude Code

The shim adds background prompts and question answering. From a clone, on macOS and Linux:

```bash
./shim.sh install
./shim.sh status
./shim.sh uninstall
```

From an installed build, the agent does the same thing without a clone:

```bash
vsc-relay-agent shim-install
vsc-relay-agent shim-status
vsc-relay-agent shim-uninstall
```

On Windows, `.\shim.ps1 install|status|uninstall`. Only chats started after the install use it.

## When something looks wrong

**Two daemons, or none.** On macOS, compare `./launchd.sh status` with the lock file; if the
app and launchd both try to own the daemon, uninstall the launchd job or stop the app. On
Linux, compare `systemctl --user status vsc-relay` with the lock file; if you pressed Start in
the GUI while the service was up, the GUI has already killed the service's daemon, so pick one
and stop the other:

```bash
systemctl --user stop vsc-relay
cat ~/.vsc-relay/vsc-relay-agent.lock
```

**A flood of approvals.** Ask which rule is firing:

```bash
vsc-relay-agent decisions --since 24h --by danger_pattern
```

The Telegram card also names the matching pattern on the line beginning with a magnifier.

**Telegram has gone quiet.** Look for a route switch in the log:

```bash
grep 'stage="route"' ~/.vsc-relay/agent.log | tail
```

**The log is growing fast.** Check the census lines first. A large `unknown_shape` count for
`Bash` means the gate cannot read most commands and is recording that fact repeatedly.

**The log looks empty on Linux.** You are probably reading the journal. The daemon logs to
`~/.vsc-relay/agent.log`; see "Reading the log" above.

**A CLI agent is "not found" only under systemd.** `systemctl --user show-environment` carries a
minimal `PATH` — roughly `/usr/local/bin:/usr/bin:/bin` — with no `~/.local/bin` and no nvm
node, so a node-shebang CLI that works in your shell is invisible to the service. The agent
compensates: it searches `PATH` plus `~/.local/bin`, `~/bin`, `~/.npm-global/bin`,
`$NPM_CONFIG_PREFIX/bin`, `~/.volta/bin`, `~/.bun/bin`, `~/.deno/bin`, `~/.cargo/bin`,
`~/.yarn/bin`, the newest `~/.nvm/versions/node/*/bin`, `/usr/local/bin`, `/opt/homebrew/bin`
and `/snap/bin`, and passes that augmented `PATH` to every CLI it spawns. If your CLI lives
somewhere else entirely, symlink it into `~/.local/bin` rather than editing the unit.

**Hand off cannot open a terminal on Linux.** A CLI handoff opens a real terminal window and
tries, in order, `x-terminal-emulator`, `ptyxis`, `kgx`, `gnome-terminal`, `konsole`,
`xfce4-terminal`, `mate-terminal`, `tilix`, `terminator`, `alacritty`, `wezterm`, `kitty`,
`foot`, `lxterminal`, `deepin-terminal`, `qterminal`, `urxvt`, `st`, `xterm`. With none of them
installed, or with no graphical session at all, it fails and names the launcher script it
prepared, which you can run yourself. Editor handoffs are found on `PATH` instead: `code`,
`code-insiders`, `code-oss`, `codium`, `vscodium`, `cursor`, `windsurf`, `antigravity`.

**`/focus` does nothing on Linux.** Window focus and the GUI fallback need an X11 or XWayland
session plus `xdotool`; `xclip` and `xdg-utils` are worth having too. The background shim path —
send, answer questions, permissions, model, effort, mode — needs no display at all and keeps
working regardless.
