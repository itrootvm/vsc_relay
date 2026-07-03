#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

find_dir() {
  ls -d \
    "$HOME"/.vscode/extensions/anthropic.claude-code-*/resources/native-binary \
    "$HOME"/.vscode-insiders/extensions/anthropic.claude-code-*/resources/native-binary \
    "$HOME"/.vscode-oss/extensions/anthropic.claude-code-*/resources/native-binary \
    "$HOME"/.vscodium/extensions/anthropic.claude-code-*/resources/native-binary \
    "$HOME"/.cursor/extensions/anthropic.claude-code-*/resources/native-binary \
    "$HOME"/.windsurf/extensions/anthropic.claude-code-*/resources/native-binary \
    2>/dev/null | sort -V | tail -1
}

DIR="$(find_dir || true)"
[ -z "${DIR:-}" ] && { echo "claude-code extension native-binary dir not found"; exit 1; }
CLAUDE="$DIR/claude"
REAL="$DIR/claude.real"
SHIM="$PWD/target/release/vsc-claude-shim"

case "${1:-status}" in
  install)
    [ -f "$SHIM" ] || { echo "build first: cargo build --release -p relay-shim"; exit 1; }
    if [ ! -f "$REAL" ]; then
      sz=$(wc -c < "$CLAUDE" | tr -d ' ')
      if [ "$sz" -lt 50000000 ]; then
        echo "REFUSING: $CLAUDE is $sz bytes (<50MB) — not the real binary. Aborting to avoid corruption."
        exit 1
      fi
      mv "$CLAUDE" "$REAL"
      echo "moved real binary → $(basename "$REAL")"
    else
      echo "claude.real already present (real binary preserved)"
    fi
    cp "$SHIM" "$CLAUDE.tmp.$$"
    chmod +x "$CLAUDE.tmp.$$"
    mv -f "$CLAUDE.tmp.$$" "$CLAUDE"
    echo "installed shim (atomic) → $CLAUDE"
    echo "open a NEW Claude chat in VS Code to pick it up (existing chats unaffected)"
    ;;
  uninstall)
    if [ -f "$REAL" ]; then
      mv -f "$REAL" "$CLAUDE"
      echo "restored real binary → $CLAUDE"
    else
      echo "no claude.real found; nothing to restore"
    fi
    ;;
  status)
    echo "dir: $DIR"
    for f in claude claude.real; do
      if [ -f "$DIR/$f" ]; then
        echo "$f: $(wc -c < "$DIR/$f" | tr -d ' ') bytes | $(file -b "$DIR/$f" | cut -c1-45)"
      else
        echo "$f: absent"
      fi
    done
    ;;
  *)
    echo "usage: shim.sh install|uninstall|status"
    exit 1
    ;;
esac
