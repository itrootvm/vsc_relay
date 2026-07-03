# Security Policy

## Supported Versions

The current supported public version is `0.1.4`. Older builds should be upgraded before
reporting a defect unless the issue is specifically about upgrade or uninstall behavior.

## Reporting Vulnerabilities

Use GitHub private vulnerability reporting if it is enabled for this repository. If it is
not enabled, open a GitHub issue with a minimal description and do not include tokens,
pairing keys, chat IDs, logs with secrets, or exploit details.

Useful reports include:

- affected version and macOS version;
- whether the app, terminal service, or shim was used;
- whether the issue requires a paired Telegram chat;
- a short reproduction path;
- logs with secrets removed.

## Local Data Model

VSC Relay runs locally on your Mac. It does not run a hosted backend for relay traffic.

- The macOS app stores the Telegram bot token and pairing key in Keychain.
- Terminal mode reads `TELEGRAM_BOT_TOKEN`, `RELAY_PAIR_SECRET`, and optional chat allow
  lists from `.env`.
- Runtime state, logs, sockets, authorized chats, and danger patterns live under
  `~/.vsc-relay`.
- Claude Code and Codex session state is read from local files those tools already write.
- Local control uses Unix sockets under the current user account.

## Telegram Data Flow

The relay talks to Telegram Bot API by outbound HTTPS long polling. A Telegram chat cannot
control the relay until it is paired with `/auth <key>` or explicitly allowed through
`TELEGRAM_ALLOWED_CHATS`.

After pairing, Telegram can receive session summaries and can send control actions such as
prompts, stop requests, focus requests, permission answers, and blocked-command list
changes. Treat the paired Telegram chat as a control surface for your local coding agents.

## Safe Uninstall

1. Stop the relay from the app or run `./svc.sh stop`.
2. Remove the Claude Code shim from the app or run `./shim.sh uninstall`.
3. Remove app state with `rm -rf ~/.vsc-relay` if you no longer need logs or pairing data.
4. Remove terminal secrets from `.env` if terminal mode was used.
5. Delete the Keychain items from the app settings or Keychain Access if the app was used.

## Shim Recovery

The shim installer moves the original Claude Code helper to `claude.real` and installs
`vsc-claude-shim` as `claude`. If the extension updates, new chats may need the shim
installed again.

If a chat cannot start after shim installation:

1. Run `./shim.sh status`.
2. Run `./shim.sh uninstall`.
3. Restart VS Code.
4. If needed, reinstall or update the Claude Code VS Code extension.

The installer refuses to replace a file that does not look like the real Claude Code helper.

## Current Limitations

These items are known limitations for `0.1.4` and are tracked as follow-up hardening work:

- no policy engine for per-command allow or deny rules;
- no fail-closed guard mode if hooks are unavailable;
- no append-only audit journal;
- no socket peer credential checks beyond local filesystem isolation;
- no automatic redaction layer for transcript text sent to Telegram;
- callback payloads are compact but not yet redesigned for every unusual workspace name;
- Codex support is read and focus oriented, not full background control.

Use a strong pairing key, keep `.env` private, review the paired chat list, and avoid
pairing bots in shared Telegram groups.
