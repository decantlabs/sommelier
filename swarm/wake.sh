#!/bin/sh
# Wakes the coordinator, and as seldom as it can: every wake costs a turn of the most costly model.
# At once: a landing result, STUCK or RUN_RESULT on the bus, and a READY only when it reached the
# inbox, which bus-listen.sh allows only for a READY that ready-queue.sh could not resolve to a
# branch (all others are queued for landing and answered with an ACCEPT, no wake up). Everything
# else (watchdog reports, FIX, HELLO, chat on quota and capacity) is collected and shown in one
# batch, not sooner than BATCH seconds after the start (default 20 minutes).
# WAKE_ON_RUN=1 also wakes when the running game run ends. It prints what woke it. Start it in the
# background and read its output when it exits.
#
#   wake.sh
#
# Files: $SOMMELIER_RUNS/watchdog.findings, $SOMMELIER_RUNS/bus.inbox, the game lock folder $SOMMELIER_RUNS/.lock.
# Settings: swarm/env.sh.
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
RUNS=$SOMMELIER_RUNS; F=$RUNS/watchdog.findings; I=$RUNS/bus.inbox
mkdir -p "$RUNS"; touch "$F" "$I"
f0=$(wc -l < "$F"); i0=$(wc -l < "$I")
BATCH=${BATCH:-1200}; t0=$(date +%s)
run=0; [ "${WAKE_ON_RUN:-0}" = 1 ] && [ -d "$RUNS/.lock" ] && run=1
show() {
  [ "$(wc -l < "$F")" -gt "$f0" ] && { echo "FINDING:"; tail -n +$((f0+1)) "$F" | cut -c1-400; }
  [ "$(wc -l < "$I")" -gt "$i0" ] && { echo "BUS:"; tail -n +$((i0+1)) "$I" | cut -c1-900; }
}
while :; do sleep 20
  newf=$(tail -n +$((f0+1)) "$F"); newi=$(tail -n +$((i0+1)) "$I")
  if echo "$newf" | grep -qE "^LANDER " || echo "$newi" | grep -qE "^#[0-9]+ [0-9:]+ [^ ]+ (READY|STUCK|RUN_RESULT) "; then show; break; fi
  if [ -n "$newf$newi" ] && [ $(( $(date +%s) - t0 )) -ge "$BATCH" ]; then show; break; fi
  if [ $run = 1 ] && [ ! -d "$RUNS/.lock" ]; then echo "RUN ENDED: game lock free"; break; fi
done
