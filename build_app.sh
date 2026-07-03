#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

APP="VSCRelay.app"
BUILD="build"
CONTENTS="$BUILD/$APP/Contents"

echo "1/5 building rust release binaries..."
cargo build --release >/dev/null

echo "2/5 assembling app bundle..."
rm -rf "$BUILD/$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp macapp/Info.plist "$CONTENTS/Info.plist"

for b in vsc-relay-agent vsc-claude-shim; do
  cp "target/release/$b" "$CONTENTS/Resources/$b"
  strip "$CONTENTS/Resources/$b" 2>/dev/null || true
  chmod +x "$CONTENTS/Resources/$b"
done

echo "3/5 compiling swift ui..."
swiftc -O -parse-as-library \
  -target arm64-apple-macosx14.0 \
  -o "$CONTENTS/MacOS/VSCRelay" \
  macapp/VSCRelay/main.swift \
  -framework SwiftUI -framework AppKit -framework Foundation

echo "4/5 signing (ad-hoc)..."
codesign --force --deep --sign - "$BUILD/$APP" >/dev/null 2>&1 || echo "  (ad-hoc sign skipped)"

echo "5/5 packaging dmg with install instructions..."
STAGE="$BUILD/dmg-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -R "$BUILD/$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
cat > "$STAGE/INSTALL.txt" <<'TXT'
VS Code Agent Relay - install

1. Drag VSCRelay.app onto the Applications folder in this window.
2. Open Applications and launch VSCRelay. The first time, right click it
   and choose Open so macOS allows an app from outside the App Store.
3. Click Settings, paste your Telegram bot token, set a pairing key,
   then click Start.
4. In Telegram, send /auth <your key> to your bot, then /menu.

No Rust or extra tools are required to run this.
TXT

if command -v hdiutil >/dev/null 2>&1; then
  rm -f "$BUILD/VSCRelay.dmg"
  hdiutil create -quiet -volname "VS Code Agent Relay" \
    -srcfolder "$STAGE" -ov -format UDZO "$BUILD/VSCRelay.dmg"
  echo "dmg:  $BUILD/VSCRelay.dmg"
fi
rm -rf "$STAGE"

SIZE=$(du -sh "$BUILD/$APP" | awk '{print $1}')
echo "app:  $BUILD/$APP ($SIZE)"
echo "note: built for Apple Silicon (arm64)."
echo "done. open with: open \"$BUILD/$APP\""
