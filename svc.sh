#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

RELAY_DIR="$HOME/.vsc-relay"
PIDFILE="$RELAY_DIR/agent.pid"
LOGFILE="$RELAY_DIR/agent.log"
BIN="$PWD/target/release/vsc-relay-agent"
mkdir -p "$RELAY_DIR"

load_env() {
  if [[ -f .env ]]; then
    set -a
    . ./.env
    set +a
  fi
}

daemon_pids() {
  { pgrep -f "vsc-relay-agent" 2>/dev/null || true; } | while read -r p; do
    cmd=$(ps -o command= -p "$p" 2>/dev/null || true)
    case "$cmd" in
      *" hook "*|*"svc.sh"*) ;;
      *vsc-relay-agent*) echo "$p" ;;
    esac
  done
}

start() {
  local pids
  pids="$(daemon_pids)"
  if [[ -n "$pids" ]]; then
    echo "🟢 already running (pid $(echo "$pids" | tr '\n' ' '))"
    return 0
  fi
  load_env
  if [[ -z "${TELEGRAM_BOT_TOKEN:-}" ]]; then
    echo "TELEGRAM_BOT_TOKEN is not set."
    echo "Create $PWD/.env from .env.example (TELEGRAM_BOT_TOKEN, RELAY_PAIR_SECRET)."
    exit 1
  fi
  echo "building release…"
  cargo build --release >/dev/null
  nohup "$BIN" >>"$LOGFILE" 2>&1 &
  echo $! >"$PIDFILE"
  sleep 1
  status
}

stop() {
  local pids
  pids="$(daemon_pids)"
  if [[ -z "$pids" ]]; then
    echo "🔴 not running"
    rm -f "$PIDFILE"
    return 0
  fi
  echo "$pids" | xargs -I{} kill {} 2>/dev/null || true
  sleep 1
  pids="$(daemon_pids)"
  if [[ -n "$pids" ]]; then
    echo "$pids" | xargs -I{} kill -9 {} 2>/dev/null || true
  fi
  rm -f "$PIDFILE"
  echo "⏹ stopped"
}

status() {
  local pids
  pids="$(daemon_pids)"
  if [[ -n "$pids" ]]; then
    echo "🟢 running (pid $(echo "$pids" | tr '\n' ' '))"
  else
    echo "🔴 not running"
  fi
  echo "log: $LOGFILE"
  tail -n 12 "$LOGFILE" 2>/dev/null || true
}

case "${1:-status}" in
  start) start ;;
  stop) stop ;;
  restart)
    stop
    sleep 1
    start
    ;;
  status) status ;;
  logs) tail -n 50 -f "$LOGFILE" ;;
  *) echo "usage: ./svc.sh {start|stop|restart|status|logs}" ;;
esac
