#!/bin/sh
# The coordinator's ear on the bus: ONE connection that stays open (so the orchestrator shows as
# online), and every message that needs the coordinator is appended to $SOMMELIER_RUNS/bus.inbox.
# A separate waiter wakes the coordinator when that file grows. Never exits by itself.
#
# Lean mode (budget): only STUCK, RUN_RESULT, FIX and HELLO reach the inbox, plus any message whose
# topic is quota or capacity, plus a READY that ready-queue.sh could not resolve to a branch. A READY
# with a branch is queued for landing and answered with an ACCEPT, the coordinator is not woken.
# Chats, declines and claims are seen in the coordinator's rounds.
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
sommelier_need SOMMELIER_URL "the hub, as wss://host"
RUNS=$SOMMELIER_RUNS; IN=$RUNS/bus.inbox; ST=$RUNS/.bus.listen.last
ME=$SOMMELIER_ORCH
export SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN"
mkdir -p "$RUNS"; touch "$IN"
since=${1:-$(cat "$ST" 2>/dev/null || echo 1)}
"$SOMMELIER_BIN" tail --since "$since" 2>&1 | while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^#\([0-9][0-9]*\) .*/\1/p')
  [ -z "$id" ] && continue
  echo $((id + 1)) > "$ST"
  set -- $line
  from=$3; kind=$4
  [ "$from" = "$ME" ] && continue
  # READY: ready-queue.sh queues the branch and answers on the bus. Only a READY it could not
  # resolve reaches the inbox (and wakes the coordinator). stdin is closed so it cannot eat the tail.
  want=0
  case "$kind" in
    STUCK|RUN_RESULT|FIX|HELLO) want=1 ;;
    READY) sh "$HERE/ready-queue.sh" "$line" </dev/null >/dev/null 2>&1 || want=1 ;;
  esac
  case "$line" in *" $kind quota "*|*" $kind capacity "*) want=1 ;; esac
  [ $want = 1 ] && printf '%s\n' "$line" | cut -c1-900 >> "$IN"
done
