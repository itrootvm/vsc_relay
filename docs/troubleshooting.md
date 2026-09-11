# Troubleshooting

A heading that names a platform applies only to that platform. Everything else applies
everywhere.

## Where The Log Is

The daemon writes and rotates its own log file on every platform:

```bash
tail -f ~/.vsc-relay/agent.log
```

On Windows the same file is `%USERPROFILE%\.vsc-relay\agent.log`. Older generations are
`agent.log.1` through `agent.log.12`.

On Linux this is the instruction most often given wrong. The tracing output goes to that
file, not to stdout, so the systemd user service has almost nothing in the journal:
`journalctl --user -u vsc-relay -f` shows the unit starting, stopping and crashing and
little else. Anything about sessions, cards, routes or gates is in the file:

```bash
tail -f ~/.vsc-relay/agent.log
grep 'stage="route"' ~/.vsc-relay/agent.log | tail
```

Use `journalctl --user -u vsc-relay` for the unit's own lifecycle, and `agent.log` for what
the relay did. The Linux and Windows GUI tails the same file in its Diagnostics pane, with a
text filter and a **Gate trace only** toggle, so you can read it without a terminal.

## Telegram Says Unauthorized

Send `/auth <key>` again from the private chat you want to use. Make sure the key matches
the app setting or `RELAY_PAIR_SECRET` in `.env`.

## Telegram Goes Quiet After Rebuilding the App (macOS)

This one is macOS only; it is about the Keychain, which no other platform uses.

The GUI keeps the bot token in the macOS Keychain. The Keychain grants access by code
signature, and the local build signs the app ad hoc, so every rebuild produces a new
signature that the stored token no longer trusts. On the next launch the GUI reads an empty
token and starts the agent with `telegram=false`, and no messages are sent or received. The
log line `vsc-relay-agent starting ... telegram=false` confirms it.

To restore service without the Keychain, run the agent from the console with the token in
`.env`:

    ./svc.sh start

Quit the GUI first so only one agent runs, otherwise both poll Telegram and conflict. Confirm
with `./svc.sh status` that exactly one agent is up and that the log shows `telegram=true`.
Re opening the token in the GUI once and approving the Keychain prompt also re authorizes the
new signature.

## The Bot Went Silent And The Relay Is Not Running At All

Check before anything else whether the process is there:

```bash
pgrep -x vsc-relay-agent || echo "not running"
```

A relay that is not running sends nothing and logs nothing, so the last log line is simply the
last thing that happened before it died. Nothing restarts it on its own unless something
supervises it, and that is a different thing on each platform.

**macOS.** The launchd job, from a clone:

```bash
./launchd.sh install     # keeps it alive across crashes, logouts and reboots
./launchd.sh status
./launchd.sh uninstall
```

The job reads everything it needs from `~/.config/vsc-relay/relay.env`, including the bot token,
so it does not depend on the shell that started it or on the app being open. With the job
installed, the Stop button in the app stops the process and launchd starts it again within
about ten seconds; use `./launchd.sh uninstall` when you want it to stay down.

**Linux.** A `systemd --user` unit does the same job:

```bash
systemctl --user enable --now vsc-relay
systemctl --user status vsc-relay
loginctl enable-linger "$USER"
```

The unit is `~/.config/systemd/user/vsc-relay.service` when the tarball's `install.sh` wrote
it, and `/usr/lib/systemd/user/vsc-relay.service` when it came from the `.deb`. It reads
`~/.config/vsc-relay/relay.env` for the token and the pairing key, and restarts the daemon on
failure after three seconds. `loginctl enable-linger` is what keeps it alive after you log out
of the desktop; without it the user manager and the relay stop with your session. To take it
down for good, use `systemctl --user disable --now vsc-relay`.

From a dev clone on Linux, `./systemd.sh` is the mirror of `./launchd.sh` and supervises the
binary in the clone:

```bash
./systemd.sh install
./systemd.sh status
./systemd.sh logs
./systemd.sh uninstall
```

`install` writes `~/.config/systemd/user/vsc-relay.service` with `ExecStart` pointing at
`target/release/vsc-relay-agent` in the current directory, imports `DISPLAY`, `XAUTHORITY`,
`WAYLAND_DISPLAY` and `XDG_SESSION_TYPE` into the user manager, stops a running GUI or agent
first, then enables and starts the unit; build the binary with `cargo build --release -p
relay-agent` first or it refuses. Run it from the clone's root, because it takes that path from
the working directory. `start`, `stop` and `restart` are there too, and `logs` follows
`~/.vsc-relay/agent.log`, not the journal. That unit restarts on failure after ten seconds.
It carries the same name as the packaged one, and a unit in `~/.config/systemd/user` shadows
`/usr/lib/systemd/user`, so a clone installed this way takes over from a `.deb` install until
you run `./systemd.sh uninstall`.

With no unit at all, `./svc.sh start` builds the release binary and runs it in the background,
`./svc.sh status` reports it and `./svc.sh logs` follows `agent.log`. Nothing restarts it that
way; install the unit when you want it supervised.

**Windows.** The installer registers a logon task named `VSCRelay` that starts the daemon in
your interactive session.

## Cards Arrive Twice, Late, Or Not At All

Two relays cannot share one bot. Telegram hands the long poll to whichever asked last and tells
the other:

```
WARN getUpdates: Conflict: terminated by other getUpdates request
```

Count them and keep one:

```bash
pgrep -x vsc-relay-agent
ps -o pid,ppid,command -p <pid>
```

More than one line from `pgrep` is the fault. Reading the parent tells you who started each
copy, and that reading differs per platform.

On macOS, a launchd-owned daemon has ppid 1; anything else was started by a shell or by the
app.

On Linux, ppid 1 means nothing: a process started by the `systemd --user` manager has that
manager as its parent, not `init`, and a daemon started by the GUI has `vsc-relay-gui` as its
parent. Ask systemd and the kernel instead:

```bash
systemctl --user show -p MainPID --value vsc-relay
cat /proc/<pid>/cgroup
```

The cgroup line ends in `vsc-relay.service` for the copy the unit owns, and in an
`app-...scope` for one started from the desktop or a terminal.

In any case the second copy should not survive: the daemon takes a machine lock at
`~/.vsc-relay/vsc-relay-agent.lock` and a copy that cannot claim it writes one line and exits.

```
another vsc-relay-agent already holds the machine lock; exiting so the two never share one bot
```

## The Bot Goes Silent While The Relay Keeps Running

If the log fills with `getUpdates: error sending request: operation timed out` every minute or
so, the relay is healthy and the route to Telegram is not. Check it from the same machine:

```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 8 https://api.telegram.org/
```

Then check whether a plain TCP connection to Telegram's data centre gets through. Use the
form for your platform; `nc` is not the same program on both.

On Linux, bash and `timeout` from coreutils are enough, and no netcat is needed:

```bash
timeout 4 bash -c '</dev/tcp/149.154.167.220/443' && echo open || echo blocked
```

With `netcat-openbsd` installed, `nc -z -w 4 149.154.167.220 443` does the same. Do not use
`-G` on Linux. It is a BSD flag: `netcat-openbsd` rejects it outright, which makes the check
print `blocked` while the route is perfectly fine, and `netcat-traditional` takes it as a
source-routing pointer, which is not a timeout at all.

On macOS, `nc` is the BSD one and `-G` is its connect timeout:

```bash
nc -z -G 4 149.154.167.220 443 && echo open || echo blocked
```

A `000` with the rest of the internet working means the path to Telegram is cut rather than the
relay being stuck. Point the relay through a proxy by adding one line to the `.env` the service
reads, then restart it:

```
VSC_RELAY_PROXY=socks5://127.0.0.1:1080
```

`ALL_PROXY` and `HTTPS_PROXY` are honoured too, in that order, so an existing shell setting
works without changing anything; `VSC_RELAY_PROXY` wins when both are present. HTTP, HTTPS and
SOCKS5 addresses are all accepted. The startup line in the log reports which one is in use, or
`direct` when there is none. A proxy address that cannot be parsed stops the relay at startup
with the reason rather than silently falling back to a direct connection.

A configured proxy is a second route, not the only way out. With one configured the relay starts
on it and switches to the other route after two failures in a row. While on the proxy it checks
the direct route every fifteen minutes with a harmless `getMe` call and moves back as soon as that
answers. Each move is one line in the log:

```
WARN telegram is unreachable on this route, switching from="proxy" to="direct"
INFO the direct route answered again, leaving the proxy
```

A message that fails before it reaches Telegram is not dropped. It is tried up to three times,
and because two failures switch the route, the last try goes out the other way. Count what that
saved and what was still lost:

```bash
vsc-relay-agent decisions --since 24h --by outcome | grep send_
```

Losses that remain almost always mean both routes were down at once, typically a proxy that only
answers while a VPN is connected. The proxy address is a local setting and belongs in
`~/.config/vsc-relay/relay.env` on that machine.

The other way in needs no tunnel on this machine: point the relay at a different Bot API
endpoint that can reach Telegram itself.

```
VSC_RELAY_TELEGRAM_API=https://your-endpoint.example.com
```

Both the API calls and file downloads follow that base, so a small HTTPS reverse proxy in front
of `api.telegram.org`, or a self-hosted `telegram-bot-api` server, works without any other
change. The path layout stays the same as Telegram's own, `/bot<token>/<method>` and
`/file/bot<token>/<path>`, so anything that forwards requests unchanged is enough. The bot
token travels to that endpoint, so it must be one you control.

## A Card Says Approve But Nothing Happens

Check which card it is. The interactive question card lists the answer options as buttons and
is delivered through the shim; that one works. A plain card that only says the agent needs
permission comes from the Notification hook and its buttons press Enter in the window through
Accessibility. That path cannot answer an AskUserQuestion, because the answer lives inside a
webview the platform does not expose, so tapping it does nothing visible. The relay now
suppresses that second card while an interactive one is live, and logs the reason:

```
an interactive card already owns this prompt; notification card suppressed
```

If you see the plain card alone, the session is not tapped: the shim is loaded by the editor,
so a chat started in a terminal has no send path. Answer it in the window.

## No VS Code Windows Appear

Open the workspace in VS Code and start a Claude Code or Codex chat. Then run `/windows` or
open `/menu` again.

## The Relay Can Read But Cannot Send

For Claude Code, install the shim and start a new Claude Code chat after installation.
Already-open chats keep using the helper binary they started with.

For Codex, this is expected. Codex background control is not implemented yet.

## A Session Warns That It Runs Without Remote Control

`session not tapped; permissions cannot be approved from Telegram` means the session was
started outside VS Code, typically as `claude` in a terminal. Hooks still fire, so the relay
sees the session and every tool call it makes, and the session shows up in the chat list. Only
the send path is missing, because that one goes through the shim the editor loads. Start the
chat in VS Code if you need to answer its permission prompts from Telegram.

## Focus Does Not Work

On macOS, grant Accessibility permission to the app or terminal that runs the relay. Then
restart the relay and try `/focus <workspace>` again.

On Linux, window focus and GUI fallback need an X11 (or XWayland) session and `xdotool`
(`sudo apt install xdotool xclip xdg-utils`). Under a native Wayland session the compositor
usually blocks key injection into other windows; prefer an X11/Xorg session for the GUI
fallback. If the relay runs as a systemd user service, it must inherit `DISPLAY`. The
installer runs `systemctl --user import-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY
XDG_SESSION_TYPE`; re-run it and restart the service if you started the session differently.
The background shim path does not need a display and keeps working regardless, so sending,
answering questions, permissions, and model/effort/mode work even when `/focus` does not.

On Windows, window focus and GUI fallback need an interactive desktop session. The logon task
the installer registers runs there; a Session-0 service cannot focus windows. The background
shim path over named pipes needs no session and keeps working, so sending, answering
questions, permissions, and model/effort/mode work even when `/focus` does not.

The two Linux failures read differently in the log. A missing tool says so by name:

```
required tool 'xdotool' not found; install it (e.g. sudo apt install xdotool xclip xdg-utils)
```

A daemon with no display at all says this instead, and no amount of installing helps until it
has one:

```
no X display (DISPLAY is unset); window focus / GUI control needs an X11 or XWayland session. The background shim path still controls Claude Code without a display.
```

## A CLI Agent Runs In Your Terminal But The Service Cannot Find It (Linux)

The `systemd --user` manager starts the unit with a deliberately small environment. Look at
what it actually passes:

```bash
systemctl --user show-environment
```

The `PATH` there is usually just `/usr/local/bin:/usr/bin:/bin` and the games directories. It
has no `~/.local/bin`, no `~/.npm-global/bin` and no nvm node, which is where `claude`,
`codex`, `gemini` and friends normally live, so a CLI that works in your shell is invisible to
a daemon started by systemd.

The relay compensates for this itself. It searches `PATH` plus `~/.local/bin`, `~/bin`,
`~/.npm-global/bin`, `$NPM_CONFIG_PREFIX/bin`, `~/.volta/bin`, `~/.bun/bin`, `~/.deno/bin`,
`~/.cargo/bin`, `~/.yarn/bin`, the newest `~/.nvm/versions/node/*/bin`, `/usr/local/bin`,
`/opt/homebrew/bin` and `/snap/bin`, and it hands that same augmented `PATH` (with `TERM`) to
every CLI agent it spawns. That is what makes a node-shebang CLI work under the service.

If a CLI still is not found, it lives somewhere outside that list. Either symlink it into
`~/.local/bin`, or give the unit a drop-in with a `[Service]` section that sets
`Environment=PATH=...` including its directory:

```bash
systemctl --user edit vsc-relay
systemctl --user restart vsc-relay
```

Confirm what the unit ended up with, rather than what your shell has:

```bash
systemctl --user show vsc-relay -p Environment
```

## Handing Off To A CLI Opens No Terminal (Linux)

A hand off to an agent CLI writes the prompt and a launcher script under
`~/.vsc-relay/handoff/`, then opens a terminal to run it. It tries `x-terminal-emulator`,
`ptyxis`, `kgx`, `gnome-terminal`, `konsole`, `xfce4-terminal`, `mate-terminal`, `tilix`,
`terminator`, `alacritty`, `wezterm`, `kitty`, `foot`, `lxterminal`, `deepin-terminal`,
`qterminal`, `urxvt`, `st` and `xterm`, in that order, and takes the first one it finds.

With none of them installed it says so and names the script, which you can run yourself:

```
no terminal emulator found on PATH; install one (xterm, konsole, gnome-terminal, alacritty, kitty, foot) or run ~/.vsc-relay/handoff/launch-<stamp>.sh yourself
```

With no graphical session at all, neither `DISPLAY` nor `WAYLAND_DISPLAY`, there is nowhere to
open a window, and the message names the same script:

```
no graphical session to open a terminal in; run ~/.vsc-relay/handoff/launch-<stamp>.sh yourself
```

Run that script from any terminal you do have, over SSH included, and the hand off proceeds
normally. A hand off to an editor does not need a terminal; those are found on `PATH` as
`code`, `code-insiders`, `code-oss`, `codium`, `vscodium`, `cursor`, `windsurf` and
`antigravity`. An editor that is missing is reported as `<name> is not installed or not on
PATH on this machine`, and a missing agent CLI as `<name> is not installed`.

None of this touches the background shim path. Sending, answering questions, permissions and
model/effort/mode work on a headless machine.

## The GUI And The systemd Service Fight Over The Daemon (Linux)

Run one or the other, never both. The daemon takes a single-instance lock at
`~/.vsc-relay/vsc-relay-agent.lock`, so a second copy exits immediately instead of polling the
same bot. On top of that, the GUI's Start sends `SIGTERM` to every running `vsc-relay-agent`
before starting its own, and the unit restarts the one systemd owns three seconds later, so
with both in play they take turns killing each other and losing the lock. The visible symptoms
are a service that looks enabled but owns nothing, a Start button that appears to do nothing,
and a restart count that keeps climbing in `systemctl --user status vsc-relay`.

Pick one:

```bash
systemctl --user disable --now vsc-relay
```

then use the GUI; or quit the GUI and let the unit own the daemon.

## Compass, Steer Or Gate Do Nothing (Linux)

Linux builds have no local ONNX semantic backend. The `local-onnx` cargo feature is disabled
for Linux targets because the prebuilt ONNX Runtime needs a recent glibc and has no musl build
at all, which would undo "runs on any distribution". The default semantic backend on Linux is
therefore `off`, not `local`, and a config that asks for `local` fails at classification time,
and equally under `automation smart check`, with:

```
this build has no local ONNX semantic backend (it is not available on Linux, where the prebuilt ONNX Runtime cannot link against every glibc and cannot link statically against musl at all); set smart.semantic.backend in ~/.vsc-relay/automation.json to off, ollama, openai_compatible or agent_cli
```

Check what the backend is doing, then point it at something this build can run:

```bash
vsc-relay-agent automation smart check
vsc-relay-agent automation smart provider ollama
vsc-relay-agent automation smart model <model-id>
vsc-relay-agent automation smart check
```

`check` prints `semantic backend off` when nothing is configured, `ready: ...` when the
backend answered a probe, and the failure otherwise. The model id is whatever the backend you
picked calls its model, for example the tag your Ollama server has pulled.

The providers that work on Linux are `off`, `ollama` (a localhost Ollama keeps every
transcript on the machine), `openrouter`, `nvidia` and `openai-compatible` (these send
transcript text off the machine), and the agent CLIs `claude`, `codex`, `gemini`, `cursor` and
`antigravity` (these relay it to that vendor's cloud). The relay prints a warning naming what
leaves the machine when you pick one of those. `automation smart install-local` and
`automation smart train-local` only matter for macOS and Windows builds; on Linux nothing can
run the model they produce.

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

## Health Protocol Blocks Show Up In The Session

If the session shows a `[VSC_RELAY_SESSION_HEALTH_PROTOCOL]` block, or the agent ends its
replies with a `[VSC_RELAY_HEALTH_RESULT]` envelope, that is the optional Compass feedback
protocol. It asks the agent to state its own terminal status in a fixed machine-readable
shape so the relay can tell "still working" from "claims complete" without calling a model.
It carries no instructions about the work itself and cannot override anything you asked for.

The block is delivered by the SessionStart hook as additional context. It is sent only when
the session context is built or rebuilt, that is on startup, clear, and compact. It is not
resent on resume, because the transcript already carries it and repeated copies waste context
and read as noise to the agent. Before 0.4.1 it was sent on every SessionStart including
resume; one long session accumulated 288 copies.

To turn it off entirely, open the automation menu in Telegram and toggle Feedback, or set
`smart.feedback_protocol` to `false` in `~/.vsc-relay/automation.json`. Turning off Compass
(`smart.enabled`) also disables it.

## The Machine Runs Out Of Memory During A Long Session

A single Claude Code session appends to one transcript file forever. On this machine a
long-running session reached 1.6 GB in the transcript plus 722 MB in its sidecar directory,
with individual JSON lines of 4.5 MB. Nothing rotates or prunes those files; the agent
process, the editor, and every tool that reads the transcript pay for that size.

Check the largest transcripts:

```bash
du -sh ~/.claude/projects/* | sort -h | tail
find ~/.claude/projects -name '*.jsonl' -size +200M -exec ls -lh {} \;
```

If the machine became unresponsive or the daemon simply disappeared, ask the kernel who it
killed. The record is in a different place on each platform.

**macOS.** Jetsam writes a report that names every process and its peak footprint:

```bash
ls /Library/Logs/DiagnosticReports/JetsamEvent-*.ips
```

Read the `lifetimeMax` field per process (it is in 16 KB pages) to see which process actually
grew, rather than guessing.

**Linux.** There is no Jetsam. The kernel OOM killer logs one line naming the victim, its pid
and how much it had:

```bash
sudo dmesg -T | grep -i "out of memory"
journalctl -k --since -1d | grep -i "killed process"
```

Debian and Ubuntu set `kernel.dmesg_restrict=1`, so `dmesg` needs `sudo`, and `journalctl -k`
needs membership in `adm` or `systemd-journal` (or `sudo`); without either it prints nothing
and that is not evidence of nothing happening.

If the relay ran under the systemd user unit, systemd recorded the death itself, including a
kill by signal 9:

```bash
systemctl --user status vsc-relay
systemctl --user show vsc-relay -p NRestarts -p Result -p ExecMainCode -p ExecMainStatus
journalctl --user -u vsc-relay --since -1d
```

`Result=oom-kill`, or a `Main process exited, code=killed, status=9/KILL` line, says the
machine ran out of memory rather than the relay crashing. Remember that the relay's own log
is `~/.vsc-relay/agent.log` and simply stops mid-line when the process is killed.

On any platform, total demand across all processes is what matters: on a 16 GB machine a total
of roughly 30 GB means heavy swapping, and the editor plus browser plus the agent together
usually explain it.

The relay's own contribution is bounded. It reads each transcript incrementally, caches at
most 64 transcript reducers and evicts the least recently used one, scans only newly appended
bytes for line breaks, and releases any oversized line buffer after use. Before 0.4.1 the
cache was unbounded, a partially received line was rescanned from its start on every 1 MB
chunk, and the whole partial line was JSON-parsed on every chunk, so one 4.5 MB line cost
several full parses and the buffer stayed allocated for the life of the process.

To recover space, close finished sessions and archive or delete their transcripts.

## DMG Will Not Open (macOS)

The app is ad-hoc signed. On first launch, right click `VSCRelay.app` and choose Open. If
the downloaded file looks corrupted, download it again and compare its SHA256 checksum with
the release `SHA256SUMS` file.
