# Troubleshooting

## Telegram Says Unauthorized

Send `/auth <key>` again from the private chat you want to use. Make sure the key matches
the app setting or `RELAY_PAIR_SECRET` in `.env`.

## Telegram Goes Quiet After Rebuilding the App

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
last thing that happened before it died. Nothing restarts it on its own unless the launchd job
is installed:

```bash
./launchd.sh install     # keeps it alive across crashes, logouts and reboots
./launchd.sh status
./launchd.sh uninstall
```

The job reads everything it needs from `~/.config/vsc-relay/relay.env`, including the bot token,
so it does not depend on the shell that started it or on the app being open. With the job
installed, the Stop button in the app stops the process and launchd starts it again within
about ten seconds; use `./launchd.sh uninstall` when you want it to stay down.

## Cards Arrive Twice, Late, Or Not At All

Two relays cannot share one bot. Telegram hands the long poll to whichever asked last and tells
the other:

```
WARN getUpdates: Conflict: terminated by other getUpdates request
```

Count them and keep one:

```bash
pgrep -x vsc-relay-agent          # more than one line is the fault
ps -o pid,ppid,command -p <pid>   # ppid 1 is the launchd job, anything else is a child
```

## The Bot Goes Silent While The Relay Keeps Running

If the log fills with `getUpdates: error sending request: operation timed out` every minute or
so, the relay is healthy and the route to Telegram is not. Check it from the same machine:

```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 8 https://api.telegram.org/
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

If macOS killed processes or the machine became unresponsive, the kernel writes a report
that names every process and its peak footprint:

```bash
ls /Library/Logs/DiagnosticReports/JetsamEvent-*.ips
```

Read the `lifetimeMax` field per process (it is in 16 KB pages) to see which process actually
grew, rather than guessing. Total demand across all processes is what matters: on a 16 GB
machine a total of roughly 30 GB means heavy swapping, and the editor plus browser plus the
agent together usually explain it.

The relay's own contribution is bounded. It reads each transcript incrementally, caches at
most 64 transcript reducers and evicts the least recently used one, scans only newly appended
bytes for line breaks, and releases any oversized line buffer after use. Before 0.4.1 the
cache was unbounded, a partially received line was rescanned from its start on every 1 MB
chunk, and the whole partial line was JSON-parsed on every chunk, so one 4.5 MB line cost
several full parses and the buffer stayed allocated for the life of the process.

To recover space, close finished sessions and archive or delete their transcripts.

## DMG Will Not Open

The app is ad-hoc signed. On first launch, right click `VSCRelay.app` and choose Open. If
the downloaded file looks corrupted, download it again and compare its SHA256 checksum with
the release `SHA256SUMS` file.
