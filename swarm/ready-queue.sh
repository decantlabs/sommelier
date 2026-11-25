#!/bin/sh
# Only the participants in READYQ_TRUSTED are queued by themselves.
# For others the READY goes to the coordinator, who reads the branch first.
# Queues the branch of a READY message for landing, so READY does not wake the coordinator.
# bus-listen.sh calls it for every READY from another participant:
#
#   ready-queue.sh "<the bus line of the READY>"
#
# It takes the branch names from the refs of the line, fetches the remotes of the landing checkout
# ($SOMMELIER_INTEGRATE, the clone lander.sh works in, fetch only, nothing is checked out), and looks for
# <upstream>/<branch> first (a guest's branch), then <fork>/<branch>. A ref that is already in the
# landing queue or in the running batch is not added twice. It then answers on the bus with an ACCEPT
# with an empty body that refers to the READY. The lander's guard still holds risky branches.
#
# Exit 0: a branch was found (queued or already known). Exit 1: none could be resolved, the caller
# leaves the line for the coordinator. READYQ_DRY=1 prints what would happen and changes nothing.
# SETUP BEGIN
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
RUNS=$SOMMELIER_RUNS; WT=$SOMMELIER_INTEGRATE; UP=$SOMMELIER_UPSTREAM; FORK=$SOMMELIER_FORK; BIN=$SOMMELIER_BIN
export SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN"
# SETUP END
Q=$RUNS/landing.queue
DRY=${READYQ_DRY:-0}
line=$1
[ -n "$line" ] || exit 1
id=$(printf '%s' "$line" | sed -n 's/^#\([0-9][0-9]*\) .*/\1/p')
[ -n "$id" ] || exit 1
set -f
head=${line%% : *}
set -- $head
from=$3; kind=$4
[ "$kind" = READY ] || exit 1
TRUSTED=${READYQ_TRUSTED:-}
case " $TRUSTED " in *" $from "*) ;; *) exit 1 ;; esac
[ $# -ge 4 ] && shift 4
case "${1:-}" in @*) shift ;; esac
topic=${1:--}; [ $# -ge 1 ] && shift
[ "$topic" = "-" ] && topic=""
refs="$*"
set +f
[ -d "$WT" ] || exit 1

if [ $DRY = 0 ]; then
  timeout 90 git -C "$WT" fetch -q "$UP" </dev/null >/dev/null 2>&1
  timeout 90 git -C "$WT" fetch -q "$FORK" </dev/null >/dev/null 2>&1
fi

found=""
for r in $refs; do
  case "$r" in "#"*|main|master) continue ;; esac
  # A bare commit id is no branch.
  case "$r" in *[!0-9a-fA-F]*) ;; *) [ ${#r} -ge 7 ] && continue ;; esac
  for remote in "$UP" "$FORK"; do
    if git -C "$WT" rev-parse -q --verify "refs/remotes/$remote/$r" >/dev/null 2>&1; then
      found="$found $remote/$r"; break
    fi
  done
done
[ -n "$found" ] || exit 1

for ref in $found; do
  if grep -qxF "$ref" "$Q" "$Q.batch" 2>/dev/null; then
    echo "already queued: $ref"
  elif [ $DRY = 1 ]; then
    echo "would queue: $ref"
  else
    echo "$ref" >> "$Q"
    echo "$(date '+%F %T') READY #$id from $from queued $ref" >> "$RUNS/lander.log"
    echo "queued: $ref"
  fi
done

# The answer: ACCEPT with an empty body, referring to the READY, to its sender only.
if [ $DRY = 1 ]; then
  echo "would send: ACCEPT --re $id --to $from${topic:+ --topic $topic}"
elif [ -n "$SOMMELIER_URL" ]; then
  # shellcheck disable=SC2086
  timeout 20 "$BIN" send ACCEPT --re "$id" --to "$from" ${topic:+--topic "$topic"} </dev/null >/dev/null 2>&1
fi
exit 0
