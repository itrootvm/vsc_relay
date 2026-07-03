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
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $new" macapp/Info.plist
python3 - "$new" <<'PY'
from pathlib import Path
import sys

path = Path("Cargo.toml")
version = sys.argv[1]
text = path.read_text()
prefix = "[workspace.package]\nversion = \""
start = text.index(prefix) + len(prefix)
end = text.index('"', start)
path.write_text(text[:start] + version + text[end:])
PY
echo "version: $cur -> $new"

./build_app.sh

cat <<EOF

built build/VSCRelay.app at v$new. to ship it as a GitHub release, run:

  git add VERSION Cargo.toml Cargo.lock macapp/Info.plist
  git commit -m "release v$new"
  git tag "v$new"
  git push && git push origin "v$new"

the tag fires .github/workflows/release.yml, which stamps the same v$new
(from the tag) and uploads the dmg. every running app then sees v$new as
the latest release and self-updates. one number, set once, here.
EOF
