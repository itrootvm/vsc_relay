# Security

See the repository security policy for vulnerability reporting and the full
supported-version statement:
https://github.com/itrootvm/vsc_relay/blob/main/SECURITY.md.

## Trust Boundary

VSC Relay runs on your own machine (macOS, Linux, or Windows) and controls local coding
agents. Telegram is the remote control
surface after pairing. A paired Telegram chat should be treated as trusted enough to send
prompts, stop work, answer questions, and approve or deny actions.

## Secrets

The macOS app stores the Telegram bot token and pairing key in Keychain. Terminal mode
reads secrets from `.env`. The daemon also loads `relay.env` from the config directory
(`~/.config/vsc-relay/relay.env` on Unix, `%APPDATA%\vsc-relay\relay.env` on Windows),
which is where the Linux and Windows GUI keeps them. Do not commit `.env`, `relay.env`,
logs with tokens, or screenshots containing bot tokens.

## Pairing

A chat must be paired with `/auth <key>` or allowed with `TELEGRAM_ALLOWED_CHATS` before it
can control the relay. Use a long pairing key and avoid shared Telegram groups.

## Local State

Runtime files live under `~/.vsc-relay`. This includes logs, authorized chat data, and
blocked-command patterns. Local IPC uses Unix domain sockets in that directory on macOS and
Linux and per-user named pipes on Windows. If you uninstall permanently, stop the service,
remove the shim, and remove `~/.vsc-relay` after saving any logs you need. On macOS use the
app or `./svc.sh stop` and `./shim.sh uninstall`; on Linux run
`packaging/linux/uninstall.sh` (add `--purge` to also drop `~/.vsc-relay` and the config
directory); on Windows run `packaging\windows\uninstall.ps1` and delete `~/.vsc-relay`
and `%APPDATA%\vsc-relay` (which holds `relay.env`) manually.

## Dangerous Commands

The relay includes a blocked-command guard for risky command patterns surfaced through
Claude Code hooks. The list can be viewed and changed with `/danger`, `/danger add
<pattern>`, and `/danger del <pattern>`.

This is not a complete policy engine in `0.4.0`. Treat it as a guardrail, not a sandbox.
