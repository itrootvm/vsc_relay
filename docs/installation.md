# Installation

## App Install

1. Download `VSCRelay.dmg` from the latest GitHub release.
2. Open the disk image.
3. Drag `VSCRelay.app` to Applications.
4. Launch the app. On first launch, right click and choose Open if macOS blocks it.
5. Open Settings.
6. Paste your Telegram bot token from BotFather.
7. Set a pairing key.
8. Click Start.
9. In Telegram, send `/auth <key>` to your bot.
10. Send `/menu`.

The app stores the bot token and pairing key in Keychain.

## Terminal Service

```bash
cp .env.example .env
./svc.sh start
./svc.sh status
./svc.sh logs
```

Set `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in `.env` before starting the service.

## Shim Install

Install the Claude Code shim if you want background prompts and question answers:

```bash
./shim.sh install
./shim.sh status
```

Only Claude Code chats started after shim installation use the background control channel.

Uninstall the shim:

```bash
./shim.sh uninstall
```

## Build From Source

```bash
./build_app.sh
```

The build creates `build/VSCRelay.app` and, on macOS with `hdiutil`, `build/VSCRelay.dmg`.

Current packaged builds target Apple Silicon Macs. Universal Intel support is planned.
