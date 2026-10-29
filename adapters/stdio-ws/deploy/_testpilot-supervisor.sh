#!/usr/bin/env bash
#++agent TASK-225 [17.10.2026 12:09:00]
set -u
umask 077
STATE=/home/vscode/.local/state/testpilot-adapter/gbig-pam
REPO='/workspaces/work/repos/1C Projects/GBIG PAM'
PYTHON=/home/vscode/.local/share/1c-testpilot/venv/bin/python
SERVICE="$REPO/scripts/testpilot-adapter/service.py"
CONFIG="$STATE/service.json"
mkdir -p "$STATE"
[ ! -L "$STATE" ] && [ "$(stat -c '%u:%a' "$STATE")" = "$(id -u):700" ] || exit 1
[ ! -L "$STATE/supervisor.lock" ] || exit 1
exec 9>"$STATE/supervisor.lock"
flock -n 9 || exit 0
delay=1
while :; do
  if [ ! -x "$PYTHON" ] || [ ! -f "$SERVICE" ] || [ ! -f "$CONFIG" ] || [ ! -f "$REPO/scripts/testpilot-adapter/methods-inventory.json" ]; then
    echo WAITING_DEPENDENCIES
  else
    "$PYTHON" "$SERVICE" supervise --config "$CONFIG"
    rc=$?
    # Постоянно неверная конфигурация требует исправления, не busy retry.
    [ "$rc" -ne 78 ] || { echo CONFIG_INVALID; exit 78; }
  fi
  sleep "$delay"
  [ "$delay" -ge 30 ] || delay=$((delay * 2))
  [ "$delay" -le 30 ] || delay=30
done
#++agent TASK-225
