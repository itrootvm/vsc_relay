#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

BIN_DIR="$HOME/.local/bin"
CFG_DIR="$HOME/.config/vsc-relay"
UNIT_DIR="$HOME/.config/systemd/user"
DESKTOP_DIR="$HOME/.local/share/applications"
ENV_FILE="$CFG_DIR/relay.env"

find_file() {
  local name="$1"
  for cand in "$SCRIPT_DIR/$name" "$SCRIPT_DIR/bin/$name" "$SCRIPT_DIR/../../target/release/$name"; do
    if [ -e "$cand" ]; then echo "$cand"; return 0; fi
  done
  return 1
}

AGENT="$(find_file vsc-relay-agent || true)"
SHIM="$(find_file vsc-claude-shim || true)"
GUI="$(find_file vsc-relay-gui || true)"
if [ -z "${AGENT:-}" ] || [ -z "${SHIM:-}" ]; then
  echo "error: could not find vsc-relay-agent / vsc-claude-shim next to this script"
  echo "       or under ../../target/release. Build first: cargo build --release"
  exit 1
fi

echo "1/6 installing binaries -> $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m 0755 "$AGENT" "$BIN_DIR/vsc-relay-agent"
install -m 0755 "$SHIM" "$BIN_DIR/vsc-claude-shim"
if [ -n "${GUI:-}" ]; then
  install -m 0755 "$GUI" "$BIN_DIR/vsc-relay-gui"
  echo "     gui: vsc-relay-gui"
else
  echo "     gui: not bundled (headless install)"
fi

echo "2/6 writing service -> $UNIT_DIR/vsc-relay.service"
mkdir -p "$UNIT_DIR"
install -m 0644 "$SCRIPT_DIR/vsc-relay.service" "$UNIT_DIR/vsc-relay.service"

echo "3/6 environment file -> $ENV_FILE"
mkdir -p "$CFG_DIR"
chmod 0700 "$CFG_DIR"
if [ -f "$ENV_FILE" ]; then
  echo "     keeping existing $ENV_FILE"
else
  install -m 0600 "$SCRIPT_DIR/relay.env.example" "$ENV_FILE"
  echo "     created from template"
fi

echo "4/6 desktop launcher"
if [ -n "${GUI:-}" ] && [ -f "$SCRIPT_DIR/vsc-relay.desktop" ]; then
  mkdir -p "$DESKTOP_DIR"
  sed "s#@BIN@#$BIN_DIR/vsc-relay-gui#g" "$SCRIPT_DIR/vsc-relay.desktop" > "$DESKTOP_DIR/vsc-relay.desktop"
  chmod 0644 "$DESKTOP_DIR/vsc-relay.desktop"
  update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  echo "     $DESKTOP_DIR/vsc-relay.desktop"
else
  echo "     skipped (no gui)"
fi

echo "5/6 wiring Claude Code hooks"
"$BIN_DIR/vsc-relay-agent" install-hooks || echo "     (hooks step failed; rerun: vsc-relay-agent install-hooks)"

echo "6/6 systemd user daemon-reload + import graphical env"
systemctl --user daemon-reload || true
systemctl --user import-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY XDG_SESSION_TYPE 2>/dev/null || true

cat <<EOF

installed.

GUI (any desktop): launch "VS Code Agent Relay" from your app menu, or run
  $BIN_DIR/vsc-relay-gui
Set the token + pairing key in Settings, click Start.

Headless (systemd) instead:
  \$EDITOR $ENV_FILE
  $BIN_DIR/vsc-relay-agent shim-install
  systemctl --user enable --now vsc-relay
  loginctl enable-linger "$USER"

Then in Telegram: /auth <pairing key> then /menu

status + logs:
  systemctl --user status vsc-relay
  journalctl --user -u vsc-relay -f

notes:
  - Run either the GUI or the systemd service, not both (two pollers = Telegram 409).
  - Window focus / GUI fallback needs X11 (or XWayland) + xdotool:
      sudo apt install xdotool xclip xdg-utils
  - The background shim path works without them.
EOF
