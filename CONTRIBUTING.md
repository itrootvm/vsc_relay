# Contributing

VSC Relay is a macOS Rust project with a small SwiftUI wrapper. Keep changes narrow,
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

- macOS 14 or later;
- Rust stable;
- Xcode command line tools;
- a Telegram bot token if you test the bot flow;
- Claude Code in VS Code if you test shim or background control.

Build the app and disk image:

```bash
./build_app.sh
```

Run the headless service:

```bash
cp .env.example .env
./svc.sh start
./svc.sh logs
```

## Pull Request Guidelines

- Do not commit `.env`, bot tokens, pairing keys, logs, `target/`, or `build/`.
- Keep README and docs ASCII-only.
- Update `CHANGELOG.md` for user-visible changes.
- Keep feature claims aligned with implemented behavior.
- Add or update tests when behavior changes.
- Do not broaden the product scope in cleanup PRs.

## Release Versioning

The repository version is `0.1.4`. Release version changes must keep these files in sync:

- `VERSION`;
- `Cargo.toml`;
- `Cargo.lock`;
- `macapp/Info.plist`;
- `README.md` and `CHANGELOG.md` when user-facing release text changes.

Use `./release.sh <patch|minor|major|X.Y.Z>` to prepare a release build.
