#!/usr/bin/env bash
set -euo pipefail

UNIT="vsc-relay.service"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT_PATH="$UNIT_DIR/$UNIT"
BIN="$PWD/target/release/vsc-relay-agent"
ENV_FILE="$HOME/.config/vsc-relay/relay.env"
LOG="$HOME/.vsc-relay/agent.log"

usage() {
  echo "usage: ./systemd.sh <install|uninstall|status|start|stop|restart|logs>"
  exit 1
}

require_systemd() {
  if ! command -v systemctl >/dev/null 2>&1; then
    echo "systemctl not found; this machine does not use systemd"
    echo "run the daemon directly instead: $BIN"
    exit 1
  fi
  if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "no systemd user manager for this session"
    echo "try: loginctl enable-linger \"$USER\""
    exit 1
  fi
}

install_job() {
  if [[ ! -x "$BIN" ]]; then
    echo "build it first: cargo build --release -p relay-agent"
    exit 1
  fi
  require_systemd
  mkdir -p "$UNIT_DIR" "$HOME/.vsc-relay" "$(dirname "$ENV_FILE")"
  if [[ ! -f "$ENV_FILE" ]]; then
    install -m 600 /dev/null "$ENV_FILE"
    echo "created empty $ENV_FILE; set TELEGRAM_BOT_TOKEN and RELAY_PAIR_SECRET in it"
  fi
  cat >"$UNIT_PATH" <<EOF
[Unit]
Description=VS Code Agent Relay (dev clone at $PWD)
Documentation=https://github.com/itrootvm/vsc_relay
After=graphical-session.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory=$PWD
EnvironmentFile=-$ENV_FILE
ExecStart=$BIN
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload
  systemctl --user import-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY XDG_SESSION_TYPE 2>/dev/null || true
  pkill -x vsc-relay-gui 2>/dev/null || true
  pkill -x vsc-relay-agent 2>/dev/null || true
  sleep 1
  systemctl --user enable --now "$UNIT"
  echo "installed $UNIT -> $BIN"
  sleep 2
  status_job
}

uninstall_job() {
  require_systemd
  systemctl --user disable --now "$UNIT" 2>/dev/null || true
  rm -f "$UNIT_PATH"
  systemctl --user daemon-reload
  echo "removed $UNIT"
}

status_job() {
  require_systemd
  if [[ -f "$UNIT_PATH" ]]; then
    systemctl --user --no-pager --lines=0 status "$UNIT" 2>/dev/null |
      awk '/Loaded:|Active:|Main PID:/ {print "  " $0}'
  else
    echo "  not installed"
  fi
  local pids
  pids="$(pgrep -x vsc-relay-agent || true)"
  echo "  running pids: ${pids:-none}"
  echo "  log: $LOG"
}

case "${1:-}" in
  install) install_job ;;
  uninstall) uninstall_job ;;
  status) status_job ;;
  start)
    require_systemd
    systemctl --user start "$UNIT"
    status_job
    ;;
  stop)
    require_systemd
    systemctl --user stop "$UNIT"
    status_job
    ;;
  restart)
    require_systemd
    systemctl --user restart "$UNIT"
    status_job
    ;;
  logs)
    if [[ ! -f "$LOG" ]]; then
      echo "no log yet at $LOG"
      exit 1
    fi
    tail -f "$LOG"
    ;;
  *) usage ;;
esac
