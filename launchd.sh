#!/usr/bin/env bash
set -euo pipefail

LABEL="dev.vscrelay.agent"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN="$PWD/target/release/vsc-relay-agent"
LOG="$HOME/.vsc-relay/agent-stdout.log"

usage() {
  echo "usage: ./launchd.sh <install|uninstall|status>"
  exit 1
}

install_job() {
  if [[ ! -x "$BIN" ]]; then
    echo "build it first: cargo build --release -p relay-agent"
    exit 1
  fi
  mkdir -p "$HOME/Library/LaunchAgents" "$HOME/.vsc-relay"
  cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>$BIN</string></array>
  <key>WorkingDirectory</key><string>$PWD</string>
  <key>RunAtLoad</key><true/>
  <!-- Restart a crash, but not a clean exit. A copy that finds another
       agent holding the machine lock exits 0 on purpose; KeepAlive=true
       turned that into a permanent restart loop every ThrottleInterval. -->
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF
  launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
  pkill -x vsc-relay-agent 2>/dev/null || true
  launchctl bootstrap "gui/$UID" "$PLIST"
  launchctl enable "gui/$UID/$LABEL"
  echo "installed $LABEL"
  sleep 2
  status_job
}

uninstall_job() {
  launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed $LABEL"
}

status_job() {
  if launchctl print "gui/$UID/$LABEL" >/dev/null 2>&1; then
    launchctl print "gui/$UID/$LABEL" | awk '/state = |pid = |last exit/ {print "  " $0}'
  else
    echo "  not installed"
  fi
  local pids
  pids="$(pgrep -x vsc-relay-agent || true)"
  echo "  running pids: ${pids:-none}"
}

case "${1:-}" in
  install) install_job ;;
  uninstall) uninstall_job ;;
  status) status_job ;;
  *) usage ;;
esac
