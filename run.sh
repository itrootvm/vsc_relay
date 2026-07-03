#!/usr/bin/env bash
exec "$(dirname "$0")/svc.sh" "${1:-start}"
