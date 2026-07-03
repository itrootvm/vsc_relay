# VSC Relay

VSC Relay lets you control Claude Code in VS Code from Telegram: read sessions, answer
questions, approve or deny commands, and keep coding agents moving from your phone.

It runs locally on macOS, uses outbound HTTPS to Telegram, and reads local agent session
state written by VS Code, Claude Code, and Codex.

## Start Here

- [Installation](installation.md)
- [Architecture](architecture.md)
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

Packaged builds currently target Apple Silicon Macs. Universal Intel support is planned.
