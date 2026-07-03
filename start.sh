#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

pick() {
  grep -E "$1" xx.txt 2>/dev/null | tail -1 | sed -E "s/^[^=]*=//" | tr -d "'\"" | tr -d '[:space:]'
}

TOKEN="$(pick '^export TELEGRAM_BOT_TOKEN=[0-9]+:')"
SECRET="$(pick '^export RELAY_PAIR_SECRET=')"

if [[ -z "$TOKEN" ]]; then
  echo "no real TELEGRAM_BOT_TOKEN in xx.txt"
  exit 1
fi

export TELEGRAM_BOT_TOKEN="$TOKEN"
export RELAY_PAIR_SECRET="$SECRET"

pkill -f 'vsc-relay-agent' 2>/dev/null || true
sleep 0.5
cargo build --release
echo "launching relay (token …${TOKEN: -6}, secret set: $([[ -n "$SECRET" ]] && echo yes || echo no))"
exec ./target/release/vsc-relay-agent
