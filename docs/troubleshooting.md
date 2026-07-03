# Troubleshooting

## Telegram Says Unauthorized

Send `/auth <key>` again from the private chat you want to use. Make sure the key matches
the app setting or `RELAY_PAIR_SECRET` in `.env`.

## No VS Code Windows Appear

Open the workspace in VS Code and start a Claude Code or Codex chat. Then run `/windows` or
open `/menu` again.

## The Relay Can Read But Cannot Send

For Claude Code, install the shim and start a new Claude Code chat after installation.
Already-open chats keep using the helper binary they started with.

For Codex, this is expected in `0.1.4`. Codex background control is not implemented.

## Focus Does Not Work

Grant Accessibility permission to the app or terminal that runs the relay. Then restart the
relay and try `/focus <workspace>` again.

## The Shim Looks Broken

```bash
./shim.sh status
./shim.sh uninstall
```

Restart VS Code after uninstalling. If the Claude Code extension still fails to start,
reinstall or update the extension.

## DMG Will Not Open

The app is ad-hoc signed. On first launch, right click `VSCRelay.app` and choose Open. If
the downloaded file looks corrupted, download it again and compare its SHA256 checksum with
the release `SHA256SUMS` file.
