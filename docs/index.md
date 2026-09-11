# VSC Relay

VSC Relay lets you control Claude Code in VS Code from Telegram: read sessions, answer
questions, approve or deny commands, and keep coding agents moving from your phone.

It runs locally on macOS, Linux, and Windows, uses outbound HTTPS to Telegram, and reads
local agent session state written by VS Code, Claude Code, and Codex.

## Start Here

- [Installation](installation.md) — packages and first setup on macOS, Linux and Windows
- [Operating guide](operating.md) — running it day to day, supervision, logs, modes
- [Architecture](architecture.md) — how discovery, the shim, the compass and handoff fit together
- [Security](security.md)
- [Threat model](threat-model.md)
- [Troubleshooting](troubleshooting.md)
- [FAQ](faq.md)
- [Codex support](codex-support.md)
- [LLM summary](llms.txt)
- [Full LLM context](llms-full.txt)

## Current Support

| Target | Read status | Send prompt | Answer questions | Permission actions | Model or mode controls |
| --- | --- | --- | --- | --- | --- |
| Claude Code in VS Code | Yes | Yes | Yes, with shim | Yes | Yes, with shim |
| Codex in VS Code | Experimental | GUI fallback only | No | No | No |

Packaged builds cover Apple Silicon Macs, x86_64 Linux, and x64 Windows 10/11. Universal Intel macOS support is planned.

## Platform Differences

Reading sessions, background control through the shim, Compass, the gate, the modes and cross
review work the same on all three platforms. What differs:

- **Supervision.** launchd on macOS, `systemd --user` on Linux, a logon task on Windows.
- **Where the log is.** Always a file in the relay's home directory — `~/.vsc-relay/agent.log`,
  or `%USERPROFILE%\.vsc-relay\agent.log` on Windows — including under `systemd --user`, where
  journald sees only unit start and stop lines.
- **Window focus and GUI fallback.** macOS Accessibility, an X11 or XWayland session with
  `xdotool` on Linux, an interactive desktop session on Windows.
- **The local ONNX semantic backend.** macOS and Windows only. Linux builds ship without it
  and default to `off`; use `ollama`, an OpenAI-compatible endpoint, or an agent CLI instead.
- **Handoff to an agent CLI.** macOS opens Terminal, Linux opens an installed terminal
  emulator, Windows has no terminal launcher for it.

The full per-platform table is in the project [README](https://github.com/itrootvm/vsc_relay#platform-support).
