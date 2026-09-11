# FAQ

## Is This A Cloud Service?

No. The relay runs locally on your own machine (macOS, Linux, or Windows). Telegram traffic
goes through Telegram Bot API by outbound HTTPS because Telegram bots require it.

## Does It Work Without The Shim?

Yes for reading status, notifications, focus, and GUI fallback. Claude Code background
prompts and question answers require the shim.

## Does It Support Codex?

Codex support is experimental. The relay can observe Codex sessions and focus the
right VS Code window, but it cannot answer Codex questions or control Codex in the
background.

## Can Another Model Check The Agent's Work?

Yes. Cross review asks a model from a different family, for example Codex or Antigravity for a
Claude Code chat, whether the chat still serves your last instruction, and can send a correction
back into it. Run it by hand from the chat card or the app, or let it run on a schedule in Auto
and Robot. It works the same on macOS, Linux and Windows. See the
[operating guide](operating.md).

## Can I Move A Session To Another Agent?

Yes, that is session handoff. The relay writes a brief, `HANDOFF.md`, into the workspace the
session was already working in, then hands a prompt pointing at it to the destination you pick:
another Claude chat (delivered through the shim), an agent CLI installed on the machine
(`claude`, `codex`, `agy`, `cursor-agent`), or an editor (Antigravity, Cursor, Windsurf,
VSCodium, VS Code) opened on the same project. The work never moves; only the agent changes.

On Linux both kinds work. Editors are found on `PATH` — `code`, `code-insiders`, `code-oss`,
`codium`, `vscodium`, `cursor`, `windsurf`, `antigravity` — and a CLI handoff opens a real
terminal window, trying `x-terminal-emulator`, `ptyxis`, `kgx`, `gnome-terminal`, `konsole`,
`xfce4-terminal` and a dozen more in turn. With no graphical session it fails with a message
naming the generated launcher script so you can run it in any shell yourself.

Use the **Hand off** button on a Claude Code chat card in Telegram, the desktop app, or:

```bash
vsc-relay-agent handoff destinations SESSION_ID
vsc-relay-agent handoff SESSION_ID --to DESTINATION_ID
```

## Does It Need Accessibility Permission?

Only for window focus and GUI fallback actions. Reading session state does not require
screen scraping. Accessibility is the macOS backend; on Linux the same actions use X11
(xdotool) and on Windows they use the Win32 API with an interactive desktop session.

## Where Are Logs?

Runtime state lives under `~/.vsc-relay`, and the daemon's own log is the file
`~/.vsc-relay/agent.log` on every platform (`%USERPROFILE%\.vsc-relay\agent.log` on Windows).
It rotates itself, keeping twelve generations of about two megabytes each as `agent.log.1`
through `agent.log.12`.

```bash
tail -f ~/.vsc-relay/agent.log
```

The desktop apps show the same file: on Linux and Windows it is the Diagnostics pane in
`vsc-relay-gui`, which has a text filter and a **Gate trace only** toggle.

## Why Does `journalctl --user -u vsc-relay -f` Show Nothing?

Because the daemon does not log to stdout. Its tracing output goes straight to
`~/.vsc-relay/agent.log`, so the journal only ever records unit-level events: the service
starting, stopping, or crashing. That is still worth checking when the unit will not come up,
but for anything the relay itself did, read the file:

```bash
tail -f ~/.vsc-relay/agent.log
```

## How Do I Keep It Running After I Log Out? (Linux)

Enable the user service and turn on lingering, which is what lets systemd keep your user
manager alive without an active login session:

```bash
systemctl --user enable --now vsc-relay
loginctl enable-linger "$USER"
systemctl --user status vsc-relay
```

The unit comes from the tarball installer as `~/.config/systemd/user/vsc-relay.service`, or
from the `.deb` as `/usr/lib/systemd/user/vsc-relay.service`. From a source checkout,
`./systemd.sh install` writes a unit pointing at `target/release/vsc-relay-agent` instead, and
`./systemd.sh status|start|stop|restart|logs|uninstall` manage it. On macOS the equivalent is
launchd via `./launchd.sh install`; on Windows it is the logon task that `install.ps1`
registers.

## Can I Run The systemd Service And The Desktop App At The Same Time?

No — pick one. The daemon takes a single-instance lock at
`~/.vsc-relay/vsc-relay-agent.lock`, so a second copy logs which process holds the lock and
exits rather than fighting it for the Telegram bot. The GUI's **Start** button also clears
stray daemons before starting its own. If you want the app, stop the service with
`systemctl --user disable --now vsc-relay`; if you want the service, do not press Start.

## Why Is My Semantic Backend `off` On Linux?

Because Linux builds do not contain the local ONNX backend. The prebuilt ONNX Runtime needs a
recent glibc and has no musl build at all, and shipping it would break the promise that one
Linux build runs on any distribution, so the crate is compiled without it and the default
backend on Linux is `off`. Compass, the gate and cross review still work; only the on-device
classifier is missing.

Pick one of the other backends:

```bash
vsc-relay-agent automation smart provider ollama
vsc-relay-agent automation smart provider openai-compatible
vsc-relay-agent automation smart provider claude
vsc-relay-agent automation smart check
```

`ollama` against `localhost` keeps transcript text on the machine. `openai-compatible` and the
agent-CLI providers (`claude`, `codex`, `gemini`, `cursor`, `antigravity`) send transcript text
off it, and the command prints that disclosure when you set one.
`automation smart install-local` and `automation smart train-local` build that local backend, so
they are macOS and Windows commands. Running them on Linux is worse than pointless:
`install-local` downloads a model the build cannot load, and `train-local` also sets your
provider to `local`, which then fails on every classification until you set it back.

## Do I Need xdotool On Linux?

Only for window focus and GUI fallback, which also need an X11 or XWayland session. Everything
that goes through the shim — sending prompts, answering questions, Allow/Deny on permissions,
and model, effort and permission mode — works on a machine with no display at all.

```bash
sudo apt install xdotool xclip xdg-utils
```

`xclip` and `xdg-utils` are recommended rather than required. On a native Wayland session the
compositor blocks synthetic key injection, so focus and GUI fallback stay limited there even
with `xdotool` installed.

## Why Do CLI Agents Not Start Under The systemd Service?

Because `systemctl --user show-environment` carries a minimal `PATH`
(`/usr/local/bin:/usr/bin:/bin` and friends) that does not include `~/.local/bin` or an nvm
node, so a Node-shebang CLI installed in your shell profile is invisible to the service. The
agent works around this: it looks for binaries in `PATH` plus `~/.local/bin`, `~/bin`,
`~/.npm-global/bin`, `$NPM_CONFIG_PREFIX/bin`, `~/.volta/bin`, `~/.bun/bin`, `~/.deno/bin`,
`~/.cargo/bin`, `~/.yarn/bin`, the newest `~/.nvm/versions/node/*/bin`, `/usr/local/bin`,
`/opt/homebrew/bin` and `/snap/bin`, and passes that augmented `PATH` to every CLI agent it
spawns, supplying a `TERM` as well when the service environment has none. If a CLI still is
not found, it lives somewhere outside that set — add its directory to the unit's environment
or symlink it into `~/.local/bin`.

## Can Several Machines Use One Bot?

This is planned but not a primary workflow. Use one bot per machine if you want the
least confusing setup.

## Does The App Support Intel Macs?

The current packaged app targets Apple Silicon. Universal Intel support is on the roadmap.
