# Contributing

VSC Relay is a cross-platform Rust project (macOS, Linux, Windows) with a SwiftUI
wrapper on macOS and an egui desktop GUI on Linux and Windows. Keep changes narrow,
testable, and honest about what is implemented.

## Required Checks

Run these before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo audit
```

If a check cannot run on your machine, say which check failed and why.

## Development Setup

Requirements:

- macOS 14 or later, Linux, or Windows 10 or 11 (x64);
- Rust stable;
- Xcode command line tools on macOS, or MSVC build tools on Windows; on Linux, X11 plus xdotool for the GUI or window control (the background shim path needs neither);
- a Telegram bot token if you test the bot flow;
- Claude Code in VS Code if you test shim or background control.

Build the release artifacts for your platform:

```bash
./build_app.sh          # macOS: VSCRelay.app + VSCRelay.dmg
./build_linux.sh        # Linux: static musl tarball + .deb
./build_windows.ps1     # Windows: zip + install.ps1
```

Run the headless service (macOS and Linux):

```bash
cp .env.example .env
./svc.sh start
./svc.sh logs
```

On Windows, use `.\svc.ps1 start` and `.\svc.ps1 logs`; the daemon reads
`%APPDATA%\vsc-relay\relay.env` instead of `.env`.

## Pull Request Guidelines

- Do not commit `.env`, `relay.env`, bot tokens, pairing keys, logs, `target/`, `dist/`, or `build/`.
- Keep README and docs ASCII-only.
- Update `CHANGELOG.md` for user-visible changes.
- Keep feature claims aligned with implemented behavior.
- Add or update tests when behavior changes.
- Do not broaden the product scope in cleanup PRs.

## Release Versioning

The repository version is `0.4.0`. Release version changes must keep these files in sync:

- `VERSION`;
- `Cargo.toml`;
- `Cargo.lock`;
- `macapp/Info.plist`;
- `README.md` and `CHANGELOG.md` when user-facing release text changes.

Use `./release.sh <patch|minor|major|X.Y.Z>` to prepare a release build.
