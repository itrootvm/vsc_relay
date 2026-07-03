#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")"

APP_TITLE="VS Code Agent Relay"

dialog_text() {
  local prompt="$1" default="$2" hidden="${3:-no}"
  local extra=""
  [ "$hidden" = "yes" ] && extra="with hidden answer"
  default="${default//\"/}"
  osascript <<OSA 2>/dev/null
set r to text returned of (display dialog "$prompt" default answer "$default" $extra with title "$APP_TITLE" buttons {"Cancel", "OK"} default button "OK")
return r
OSA
}

dialog_choice() {
  local prompt="$1" b1="$2" b2="$3" def="$4"
  osascript <<OSA 2>/dev/null
set r to button returned of (display dialog "$prompt" with title "$APP_TITLE" buttons {"$b1", "$b2"} default button "$def")
return r
OSA
}

dialog_info() {
  osascript <<OSA 2>/dev/null
display dialog "$1" with title "$APP_TITLE" buttons {"OK"} default button "OK"
OSA
}

fail() {
  echo "setup failed: $1"
  dialog_info "Setup did not finish.

$1" || true
  exit 1
}

env_value() {
  [ -f .env ] || return 0
  grep -E "^$1=" .env 2>/dev/null | tail -1 | sed -E "s/^$1=//"
}

if ! command -v cargo >/dev/null 2>&1; then
  dialog_info "Rust is required but was not found.

Install it from https://rustup.rs and run this installer again."
  open "https://rustup.rs" || true
  exit 1
fi

echo "== $APP_TITLE setup =="

TOKEN_DEFAULT="$(env_value TELEGRAM_BOT_TOKEN)"
TOKEN="$(dialog_text "Enter the Telegram bot token from @BotFather:" "$TOKEN_DEFAULT")" \
  || fail "cancelled at token entry"
[ -n "$TOKEN" ] || fail "token was empty"

SECRET_DEFAULT="$(env_value RELAY_PAIR_SECRET)"
if [ -z "$SECRET_DEFAULT" ] && command -v openssl >/dev/null 2>&1; then
  SECRET_DEFAULT="$(openssl rand -hex 16)"
fi
SECRET="$(dialog_text "Set the pairing secret. You will send this to the bot once as /auth <secret> to authorize your chat. Keep it long and private:" "$SECRET_DEFAULT")" \
  || fail "cancelled at secret entry"
[ -n "$SECRET" ] || fail "secret was empty"

umask 077
cat > .env <<ENV
TELEGRAM_BOT_TOKEN=$TOKEN
RELAY_PAIR_SECRET=$SECRET
TELEGRAM_ALLOWED_CHATS=
RELAY_MACHINE_NAME=
RELAY_INTERVAL=2
ENV
chmod 600 .env
echo "wrote .env (permissions 600)"

echo "building release binaries (first build can take a few minutes)..."
cargo build --release || fail "cargo build failed; see the Terminal output above"

echo "starting the relay daemon..."
./svc.sh restart || fail "could not start the daemon; see the Terminal output above"

if [ "$(dialog_choice "Install the background control shim now?

It lets Telegram write into a chat and answer questions without switching windows. It safely wraps the Claude Code helper binary and can be removed later with ./shim.sh uninstall." "Skip" "Install" "Install")" = "Install" ]; then
  echo "installing shim..."
  if ./shim.sh install; then
    echo "shim installed. Open a NEW Claude chat in VS Code to pick it up."
  else
    dialog_info "The shim step reported a problem. The relay still runs without it; you can retry later with ./shim.sh install in a terminal."
  fi
fi

if [ "$(dialog_choice "Open the Accessibility settings pane?

Grant access to Terminal (or your terminal app) so the relay can focus windows and type as a fallback. Background control through the shim does not need this." "Later" "Open" "Open")" = "Open" ]; then
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" || true
fi

dialog_info "Setup is done.

The daemon is running. In Telegram, open a chat with your bot and send:

/auth $SECRET

Then send /menu to control your VS Code chats.

To stop or check the daemon later, run ./svc.sh stop or ./svc.sh status in this folder."

echo "== setup complete =="
