# Security

See the repository security policy for vulnerability reporting and the full
supported-version statement:
https://github.com/itrootvm/vsc_relay/blob/main/SECURITY.md.

## Trust Boundary

VSC Relay runs on your Mac and controls local coding agents. Telegram is the remote control
surface after pairing. A paired Telegram chat should be treated as trusted enough to send
prompts, stop work, answer questions, and approve or deny actions.

## Secrets

The app stores the Telegram bot token and pairing key in Keychain. Terminal mode reads
secrets from `.env`. Do not commit `.env`, logs with tokens, or screenshots containing bot
tokens.

## Pairing

A chat must be paired with `/auth <key>` or allowed with `TELEGRAM_ALLOWED_CHATS` before it
can control the relay. Use a long pairing key and avoid shared Telegram groups.

## Local State

Runtime files live under `~/.vsc-relay`. This includes logs, sockets, authorized chat data,
and blocked-command patterns. If you uninstall permanently, stop the service, remove the
shim, and remove `~/.vsc-relay` after saving any logs you need.

## Dangerous Commands

The relay includes a blocked-command guard for risky command patterns surfaced through
Claude Code hooks. The list can be viewed and changed with `/danger`, `/danger add
<pattern>`, and `/danger del <pattern>`.

This is not a complete policy engine in `0.1.4`. Treat it as a guardrail, not a sandbox.
