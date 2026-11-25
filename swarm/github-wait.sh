#!/bin/sh
# For the agent that keeps GitHub current.
#
#   github-wait.sh          waits until there is news for GitHub and prints it, at most 9 minutes
#                           (then it prints NOTHING, call it again). News is a new LANDED or FAILED
#                           line of the landing queue, or a new READY, LANDED, RUN_RESULT, FINDING,
#                           FIX or STUCK on the bus. It never returns sooner than MIN_GAP seconds
#                           after the last mark, so GitHub gets one update per period, not one per message.
#   github-wait.sh news     prints the news since the last mark at once, without waiting
#   github-wait.sh mark     records "GitHub is current up to now"
#
# State: $SOMMELIER_RUNS/github-sync.state with three lines: time of the mark (epoch), number of lines of
# lander.log, newest bus id. The bus token is only handed to the sommelier client as a file name and
# never printed. Settings: swarm/env.sh (SOMMELIER_URL is needed for the bus part).
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
STATE=$SOMMELIER_RUNS/github-sync.state
LOG=$SOMMELIER_RUNS/lander.log
mkdir -p "$SOMMELIER_RUNS"; touch "$LOG"
MIN_GAP=${MIN_GAP:-1500}

bus() { [ -n "$SOMMELIER_URL" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" "$SOMMELIER_BIN" "$@"; }
newest_id() { bus log --last 1 --all --json 2>/dev/null | sed -n 's/.*"id":\([0-9]*\).*/\1/p' | tail -1; }

read_state() {
  t0=0; l0=0; b0=0
  if [ -f "$STATE" ]; then t0=$(sed -n 1p "$STATE"); l0=$(sed -n 2p "$STATE"); b0=$(sed -n 3p "$STATE"); fi
}

news() {
  read_state
  out=""
  n=$(wc -l < "$LOG")
  if [ "$n" -gt "$l0" ]; then
    out=$(tail -n +$((l0 + 1)) "$LOG" | grep -E "LANDED|FAILED|CONFLICT")
    [ -n "$out" ] && out="LANDING QUEUE:
$out"
  fi
  b=$(bus log --since $((b0 + 1)) --limit 400 --all --json 2>/dev/null | python -c '
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    m = json.loads(line)
    if m["kind"] in ("READY", "LANDED", "RUN_RESULT", "FINDING", "FIX", "STUCK"):
        refs = " ".join(m.get("refs") or [])
        print("#%s %s %s %s [%s] %s : %s" % (m["id"], m["ts"][11:16], m["from"], m["kind"], m.get("topic") or "", refs, m["body"][:900].replace("\n", " ")))
')
  [ -n "$b" ] && out="$out
BUS:
$b"
  [ -n "$out" ] && printf '%s\n' "$out"
}

case "$1" in
  mark) echo "$(date +%s)
$(wc -l < "$LOG")
$(newest_id)" > "$STATE"; echo "marked: $(tr '\n' ' ' < "$STATE")" ;;
  news) news ;;
  *) end=$(( $(date +%s) + 540 ))
     while [ "$(date +%s)" -lt "$end" ]; do
       read_state
       if [ $(( $(date +%s) - t0 )) -ge "$MIN_GAP" ]; then
         n=$(news); [ -n "$n" ] && { echo "NEWS"; printf '%s\n' "$n"; exit 0; }
       fi
       sleep 30
     done
     echo NOTHING ;;
esac
