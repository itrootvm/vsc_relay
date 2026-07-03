#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

VERSION="${APP_VERSION:-$(cat VERSION 2>/dev/null || echo 0.0.0)}"

host_arch="$(uname -m)"
case "$host_arch" in
  x86_64 | amd64) host_arch=x86_64 ;;
  aarch64 | arm64) host_arch=aarch64 ;;
  *) echo "unsupported host arch: $host_arch (expected x86_64 or aarch64)" >&2; exit 1 ;;
esac

pick_target() {
  if [ -n "${1:-}" ]; then echo "$1"; return; fi
  local musl="${host_arch}-unknown-linux-musl"
  if rustup target list --installed 2>/dev/null | grep -qx "$musl" \
     && command -v "${host_arch}-linux-musl-gcc" >/dev/null 2>&1; then
    echo "$musl"; return
  fi
  if rustup target list --installed 2>/dev/null | grep -qx "$musl" \
     && command -v musl-gcc >/dev/null 2>&1; then
    echo "$musl"; return
  fi
  echo "${host_arch}-unknown-linux-gnu"
}

TARGET="$(pick_target "${1:-}")"
arch="${TARGET%%-*}"
case "$arch" in
  x86_64 | aarch64) ;;
  *) echo "unsupported target arch: $arch" >&2; exit 1 ;;
esac

echo "1/5 building daemon + shim (target $TARGET, version $VERSION)"
rustup target add "$TARGET" >/dev/null 2>&1 || true
cargo build --release --target "$TARGET" -p relay-agent -p relay-shim >/dev/null

echo "2/5 building gui (host target, links system X11/GL)"
GUI_BUILT=0
if cargo build --release -p relay-gui >/dev/null 2>&1; then
  GUI_BUILT=1
else
  echo "     gui skipped (missing desktop dev libs; daemon+shim unaffected)"
fi

OUTDIR="target/$TARGET/release"
GUIDIR="target/release"
STAGE="dist/vsc-relay-$VERSION-linux-$arch"
TARBALL="dist/vsc-relay-$VERSION-linux-$arch.tar.gz"

echo "3/5 staging bundle -> $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"
for b in vsc-relay-agent vsc-claude-shim; do
  install -m 0755 "$OUTDIR/$b" "$STAGE/bin/$b"
  strip "$STAGE/bin/$b" 2>/dev/null || true
done
if [ "$GUI_BUILT" = 1 ] && [ -x "$GUIDIR/vsc-relay-gui" ]; then
  install -m 0755 "$GUIDIR/vsc-relay-gui" "$STAGE/bin/vsc-relay-gui"
  strip "$STAGE/bin/vsc-relay-gui" 2>/dev/null || true
fi
install -m 0755 packaging/linux/install.sh "$STAGE/install.sh"
install -m 0755 packaging/linux/uninstall.sh "$STAGE/uninstall.sh"
install -m 0644 packaging/linux/vsc-relay.service "$STAGE/vsc-relay.service"
install -m 0644 packaging/linux/vsc-relay.desktop "$STAGE/vsc-relay.desktop"
install -m 0644 packaging/linux/relay.env.example "$STAGE/relay.env.example"
echo "$VERSION" > "$STAGE/VERSION"

cat > "$STAGE/README.txt" <<EOF
VS Code Agent Relay $VERSION - Linux

GUI:
  ./install.sh
  ~/.local/bin/vsc-relay-gui

Or headless (systemd):
  ./install.sh
  \$EDITOR ~/.config/vsc-relay/relay.env
  ~/.local/bin/vsc-relay-agent shim-install
  systemctl --user enable --now vsc-relay
Then in Telegram: /auth <pairing key> then /menu

Window focus / GUI fallback needs X11 (or XWayland) and xdotool:
  sudo apt install xdotool xclip xdg-utils
  sudo dnf install xdotool xclip xdg-utils
The background shim path needs none of them.

Uninstall: ./uninstall.sh   (add --purge to also remove config + state)
EOF

echo "4/5 packaging tarball -> $TARBALL"
mkdir -p dist
tar -C dist -czf "$TARBALL" "vsc-relay-$VERSION-linux-$arch"

echo "5/6 checksum"
( cd dist && sha256sum "$(basename "$TARBALL")" > "$(basename "$TARBALL").sha256" )
cat "$TARBALL.sha256"

echo "6/6 .deb"
if command -v dpkg-deb >/dev/null 2>&1; then
  DEB="$(packaging/linux/build-deb.sh "$VERSION" "$arch" "$STAGE/bin" dist)"
  ( cd dist && sha256sum "$(basename "$DEB")" > "$(basename "$DEB").sha256" )
  echo "deb: $DEB"
else
  echo "     skipped (dpkg-deb not found; tarball still built)"
fi

rm -rf "$STAGE"
SIZE=$(du -sh "$TARBALL" | awk '{print $1}')
echo "done: $TARBALL ($SIZE), daemon linked against $(basename "$TARGET"), gui_included=$GUI_BUILT"
