#!/usr/bin/env bash
#++agent TASK-225 [17.10.2026 12:09:00]
# Постоянный SDK не запускает native TestClient.
set -eu
STATE=/home/vscode/.local/state/testpilot-adapter/gbig-pam
umask 077
mkdir -p "$STATE"
[ ! -L "$STATE" ] && [ "$(stat -c '%u:%a' "$STATE")" = "$(id -u):700" ] || exit 1
PYTHON=/home/vscode/.local/share/1c-testpilot/venv/bin/python
SERVICE='/workspaces/work/repos/1C Projects/GBIG PAM/scripts/testpilot-adapter/service.py'
if [ -x "$PYTHON" ] && [ -f "$SERVICE" ]; then
  if "$PYTHON" "$SERVICE" pause-status; then
    :
  else
    rc=$?
    [ "$rc" -ne 3 ] || exit 0
    exit "$rc"
  fi
fi
setsid nohup /usr/local/bin/_testpilot-supervisor.sh </dev/null >>"$STATE/supervisor.log" 2>&1 &
#++agent TASK-225
