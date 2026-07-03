#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

cur="$(cat VERSION 2>/dev/null || echo 0.0.0)"

bump() {
  IFS=. read -r MA MI PA <<<"$1"
  case "$2" in
    major) echo "$((MA + 1)).0.0" ;;
    minor) echo "$MA.$((MI + 1)).0" ;;
    patch) echo "$MA.$MI.$((PA + 1))" ;;
  esac
}

arg="${1:-}"
case "$arg" in
  major | minor | patch) new="$(bump "$cur" "$arg")" ;;
  "")
    echo "current version: $cur"
    echo "usage: $0 <patch|minor|major|X.Y.Z>"
    exit 1
    ;;
  *)
    if [[ "$arg" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      new="$arg"
    else
      echo "bad version: $arg (want patch/minor/major or X.Y.Z)"
      exit 1
    fi
    ;;
esac

echo "$new" >VERSION
python3 - "$new" <<'PY'
from pathlib import Path
import sys

version = sys.argv[1]

cargo = Path("Cargo.toml")
text = cargo.read_text()
prefix = "[workspace.package]\nversion = \""
start = text.index(prefix) + len(prefix)
end = text.index('"', start)
cargo.write_text(text[:start] + version + text[end:])

plist = Path("macapp/Info.plist")
if plist.exists():
    import plistlib
    data = plistlib.loads(plist.read_bytes())
    data["CFBundleShortVersionString"] = version
    plist.write_bytes(plistlib.dumps(data))
PY
echo "version: $cur -> $new"

os="$(uname -s)"
case "$os" in
  Darwin) ./build_app.sh; artifact="build/VSCRelay.app + build/VSCRelay.dmg" ;;
  Linux)  ./build_linux.sh; artifact="dist/vsc-relay-$new-linux-*.tar.gz" ;;
  *) echo "note: no packaging step for $os; binaries via 'cargo build --release'"; artifact="(none)" ;;
esac

cat <<EOF

built $artifact at v$new. to ship it as a GitHub release, run:

  git add VERSION Cargo.toml Cargo.lock macapp/Info.plist
  git commit -m "release v$new"
  git tag "v$new"
  git push && git push origin "v$new"

the tag fires .github/workflows/release.yml, which builds on both macOS and
Linux runners, stamps the same v$new (from the tag), and uploads the dmg and
the linux tarball. every running macOS app then sees v$new and self-updates.
one number, set once, here.
EOF
