# Security Policy

## Supported Versions

The current supported public version is `0.4.0`. Older builds should be upgraded before
reporting a defect unless the issue is specifically about upgrade or uninstall behavior.

## Reporting Vulnerabilities

Use GitHub private vulnerability reporting if it is enabled for this repository. If it is
not enabled, open a GitHub issue with a minimal description and do not include tokens,
pairing keys, chat IDs, logs with secrets, or exploit details.

Useful reports include:

- affected version and operating system (macOS, Linux, or Windows);
- whether the app or GUI, terminal service, or shim was used;
- whether the issue requires a paired Telegram chat;
- a short reproduction path;
- logs with secrets removed.

## Local Data Model

VSC Relay runs locally on your own machine (macOS, Linux, or Windows). It does not run a
hosted backend for relay traffic.

- The macOS app stores the Telegram bot token and pairing key in Keychain.
- The daemon also loads `relay.env` from the platform config directory:
  `~/.config/vsc-relay/relay.env` on macOS and Linux, and `%APPDATA%\vsc-relay\relay.env`
  on Windows (tightened with `icacls` on Windows). The Linux and Windows GUI writes the bot
  token and pairing key there.
- Terminal mode reads `TELEGRAM_BOT_TOKEN`, `RELAY_PAIR_SECRET`, and optional chat allow
  lists from `.env`.
- Runtime state, logs, authorized chats, and danger patterns live under `~/.vsc-relay`.
- Claude Code and Codex session state is read from local files those tools already write.
- Local control uses Unix domain sockets under the current user account on macOS and Linux,
  and per-user named pipes (`PIPE_REJECT_REMOTE_CLIENTS`) on Windows. There is no inbound
  network port.

## Telegram Data Flow

The relay talks to Telegram Bot API by outbound HTTPS long polling. A Telegram chat cannot
control the relay until it is paired with `/auth <key>` or explicitly allowed through
`TELEGRAM_ALLOWED_CHATS`.

After pairing, Telegram can receive session summaries and can send control actions such as
prompts, stop requests, focus requests, permission answers, and blocked-command list
changes. Treat the paired Telegram chat as a control surface for your local coding agents.

## Safe Uninstall

1. Stop the relay from the app or run `./svc.sh stop` (`.\svc.ps1 stop` on Windows, or
   `systemctl --user stop vsc-relay` on Linux).
2. Remove the Claude Code shim from the app or run `./shim.sh uninstall` (`.\shim.ps1
   uninstall` on Windows). Packaged installs can run `packaging/linux/uninstall.sh` on
   Linux or `packaging\windows\uninstall.ps1` on Windows to remove the shim, service, and
   binaries.
3. Remove app state with `rm -rf ~/.vsc-relay` (delete `%USERPROFILE%\.vsc-relay` on
   Windows) if you no longer need logs or pairing data.
4. Remove terminal secrets from `.env` if terminal mode was used.
5. Delete the Keychain items from the app settings or Keychain Access if the macOS app was
   used. On Linux and Windows, remove `relay.env` from the config directory instead.

## Shim Recovery

The shim installer moves the original Claude Code helper to `claude.real` (`claude.exe` to
`claude.real.exe` on Windows) and installs `vsc-claude-shim` as `claude`. If the extension
updates, new chats may need the shim installed again.

If a chat cannot start after shim installation:

1. Run `./shim.sh status` (`.\shim.ps1 status` on Windows).
2. Run `./shim.sh uninstall` (`.\shim.ps1 uninstall` on Windows).
3. Restart VS Code.
4. If needed, reinstall or update the Claude Code VS Code extension.

The installer refuses to replace a file that does not look like the real Claude Code helper.

## Current Limitations

These items are known limitations for `0.4.0` and are tracked as follow-up hardening work:

- no policy engine for per-command allow or deny rules;
- no fail-closed guard mode if hooks are unavailable;
- no append-only audit journal;
- no socket peer credential checks beyond local filesystem isolation;
- no automatic redaction layer for transcript text sent to Telegram;
- callback payloads are compact but not yet redesigned for every unusual workspace name;
- Codex support is read and focus oriented, not full background control.

Use a strong pairing key, keep `.env` private, review the paired chat list, and avoid
pairing bots in shared Telegram groups.
