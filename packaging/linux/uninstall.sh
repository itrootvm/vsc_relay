#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
CFG_DIR="$HOME/.config/vsc-relay"
UNIT_DIR="$HOME/.config/systemd/user"
DESKTOP_DIR="$HOME/.local/share/applications"

echo "stopping + disabling service"
systemctl --user disable --now vsc-relay 2>/dev/null || true

echo "restoring Claude Code shim (if installed)"
if [ -x "$BIN_DIR/vsc-relay-agent" ]; then
  "$BIN_DIR/vsc-relay-agent" shim-uninstall 2>/dev/null || true
fi

echo "removing unit + launcher + binaries"
rm -f "$UNIT_DIR/vsc-relay.service"
rm -f "$DESKTOP_DIR/vsc-relay.desktop"
systemctl --user daemon-reload 2>/dev/null || true
update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
rm -f "$BIN_DIR/vsc-relay-agent" "$BIN_DIR/vsc-claude-shim" "$BIN_DIR/vsc-relay-gui"

if [ "${1:-}" = "--purge" ]; then
  echo "purging config + runtime state"
  rm -rf "$CFG_DIR" "$HOME/.vsc-relay"
else
  echo "kept config ($CFG_DIR) and state (~/.vsc-relay); pass --purge to remove them"
fi

echo "done. Claude Code hooks in ~/.claude/settings.json were left in place."
