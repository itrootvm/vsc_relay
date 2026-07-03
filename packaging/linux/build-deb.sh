#!/usr/bin/env bash
set -euo pipefail

VERSION="$1"
ARCH="$2"
BIN="$3"
OUT="$4"

case "$ARCH" in
  x86_64) DEBARCH=amd64 ;;
  aarch64) DEBARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
chmod 0755 "$ROOT"

mkdir -p "$ROOT/DEBIAN" "$ROOT/usr/bin" "$ROOT/usr/share/applications" \
  "$ROOT/usr/lib/systemd/user" "$ROOT/usr/share/doc/vsc-relay"

install -m 0755 "$BIN/vsc-relay-agent" "$ROOT/usr/bin/vsc-relay-agent"
install -m 0755 "$BIN/vsc-claude-shim" "$ROOT/usr/bin/vsc-claude-shim"
HAS_GUI=0
if [ -x "$BIN/vsc-relay-gui" ]; then
  install -m 0755 "$BIN/vsc-relay-gui" "$ROOT/usr/bin/vsc-relay-gui"
  HAS_GUI=1
fi

sed 's#@BIN@#/usr/bin/vsc-relay-gui#g' "$SCRIPT_DIR/vsc-relay.desktop" \
  > "$ROOT/usr/share/applications/vsc-relay.desktop"
sed 's#%h/.local/bin/vsc-relay-agent#/usr/bin/vsc-relay-agent#g' "$SCRIPT_DIR/vsc-relay.service" \
  > "$ROOT/usr/lib/systemd/user/vsc-relay.service"
install -m 0644 "$SCRIPT_DIR/relay.env.example" "$ROOT/usr/share/doc/vsc-relay/relay.env.example"

DEPENDS="libc6"
if [ "$HAS_GUI" = 1 ]; then
  DEPENDS="libc6, libgl1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libfontconfig1, libxcursor1, libxi6, libxrandr2"
fi

cat > "$ROOT/DEBIAN/control" <<EOF
Package: vsc-relay
Version: $VERSION
Architecture: $DEBARCH
Maintainer: itrootvm <noreply@github.com>
Section: devel
Priority: optional
Homepage: https://github.com/itrootvm/vsc_relay
Depends: $DEPENDS
Recommends: xdotool, xclip, xdg-utils
Description: Control Claude Code in VS Code from Telegram
 Watches local VS Code / Claude Code sessions and gives you a Telegram control
 panel to read status, reply, answer questions, and approve or deny tool
 permissions. Ships the vsc-relay-gui desktop app and a systemd --user service.
 After install, launch "VS Code Agent Relay", set your bot token and pairing key,
 and press Start. Window focus needs an X11 session with xdotool.
EOF

cat > "$ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi
if command -v systemctl >/dev/null 2>&1; then
  systemctl daemon-reload 2>/dev/null || true
fi
exit 0
EOF
chmod 0755 "$ROOT/DEBIAN/postinst"

DEB="$OUT/vsc-relay_${VERSION}_${DEBARCH}.deb"
dpkg-deb --build --root-owner-group "$ROOT" "$DEB" >/dev/null
echo "$DEB"
