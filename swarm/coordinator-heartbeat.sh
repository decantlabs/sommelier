#!/usr/bin/env bash
# Keeps the coordinator row fresh on the Sommelier bus while the coordinator works.
# Reads the first line of $SOMMELIER_RUNS/coordinator.status as the status text.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/env.sh"
sommelier_need SOMMELIER_URL "the hub, as wss://host"
export SOMMELIER_TOKEN_FILE="${SOMMELIER_TOKEN_FILE:-$SOMMELIER_ORCH_TOKEN}"
MODEL="${SOMMELIER_COORD_MODEL:-coordinator}"
BRANCH="${SOMMELIER_COORD_BRANCH:-main}"
INTERVAL="${SOMMELIER_COORD_INTERVAL:-45}"
STATUS_FILE="${SOMMELIER_COORD_STATUS_FILE:-$SOMMELIER_RUNS/coordinator.status}"
while :; do
  MSG=$(head -n 1 "$STATUS_FILE" 2>/dev/null || true)
  [ -n "$MSG" ] || MSG="coordinating repository work"
  "$SOMMELIER_BIN" status set coordinator --state running --model "$MODEL" --branch "$BRANCH" "$MSG" >/dev/null 2>&1 || true
  sleep "$INTERVAL"
done
