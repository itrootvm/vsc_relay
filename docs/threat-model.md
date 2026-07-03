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

## Practical Guidance

- Use a strong pairing key.
- Pair only a private Telegram chat you control.
- Keep `.env` private.
- Review blocked-command patterns with `/danger`.
- Remove the shim before deleting the Claude Code extension if you are troubleshooting.
- Upgrade to the latest release before reporting security issues.
