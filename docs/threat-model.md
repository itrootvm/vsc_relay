# Threat Model

## Protected Assets

- Telegram bot token.
- Pairing key and allowed chat list.
- Local agent sessions and transcript content.
- Ability to type into Claude Code or Codex.
- Ability to approve, deny, stop, continue, or focus work.
- Local project files reachable by the coding agent.

## In Scope For 0.1.4

- Preventing unpaired Telegram chats from controlling the relay.
- Keeping the relay local to the Mac with no inbound network port.
- Storing app secrets in Keychain.
- Avoiding accidental commits of local secrets.
- Blocking configured dangerous command patterns surfaced through hooks.
- Making shim install and uninstall recoverable.

## Out Of Scope For 0.1.4

- Full command policy engine.
- Fail-closed mode when hooks are unavailable.
- Append-only audit journal.
- Transcript redaction before Telegram delivery.
- Socket peer credential validation beyond local filesystem isolation.
- Complete Codex background control.

## Local IPC Isolation (per platform)

The control channels (hook ingress, shim inject, shim stdout tap) are local-only, and no
platform opens an inbound network port.

- macOS / Linux: Unix domain sockets under `~/.vsc-relay` (dir `0700`, files `0600`), isolated
  by filesystem permissions.
- Windows: named pipes `\\.\pipe\vsc-relay-*` created with a protected per-user DACL (owner +
  SYSTEM only), `PIPE_REJECT_REMOTE_CLIENTS`, and `FILE_FLAG_FIRST_PIPE_INSTANCE` so another
  process cannot pre-create the name to intercept it; clients open with `SECURITY_IDENTIFICATION`
  so a malicious server cannot impersonate the caller. Secrets live in
  `%APPDATA%\vsc-relay\relay.env`, tightened to the current account with `icacls /inheritance:r`.
  Window focus and GUI fallback require an interactive desktop session (a Session-0 service
  cannot reach the desktop).

## Practical Guidance

- Use a strong pairing key.
- Pair only a private Telegram chat you control.
- Keep `.env` private.
- Review blocked-command patterns with `/danger`.
- Remove the shim before deleting the Claude Code extension if you are troubleshooting.
- Upgrade to the latest release before reporting security issues.
