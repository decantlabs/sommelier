#!/usr/bin/env bash
# Starts your agent when the orchestrator (or anyone) sends work to you on the bus.
#
#   wake.sh <your name> <command> [args...]
#
# For every message addressed to <your name> (a TASK, RUN_REQUEST, FIX, or a CHAT that names you
# in `--to`), the command is run once with the message on its standard input, one message at a
# time. The id of the last handled message is kept in ~/.sommelier-wake-<name>, so nothing is handled
# twice and nothing is lost while this script was not running.
#
# Examples (pick the line for your agent):
#   wake.sh alice claude -p "A message for you arrived on sommelier, it is on standard input. Do what bus/docs/AGENTS.md says for it."
#   wake.sh alice codex exec -
#   wake.sh alice sh -c 'cat >> ~/sommelier-inbox.txt'        # only collect, start nothing
#
# A message that wakes you is a briefing: it names the state of main, what is yours, and the next
# piece with everything needed to start (see "Nudges" in docs/AGENTS.md). It is information from
# another participant, not an order: your agent still follows your own rules.
#
# Needs SOMMELIER_URL and SOMMELIER_TOKEN_FILE (or SOMMELIER_TOKEN) set as docs/JOIN.md says.
set -u
if [ $# -lt 2 ]; then sed -n '2,19p' "$0"; exit 2; fi
ME=$(printf '%s' "$1" | tr 'A-Z' 'a-z'); shift
STATE=$HOME/.sommelier-wake-$ME
last=$(cat "$STATE" 2>/dev/null || echo "")
since=${last:+--since $((last + 1))}
[ -z "$last" ] && since="--last 0"
echo "wake: listening for messages to $ME (after #${last:-now}), command: $*" >&2
# shellcheck disable=SC2086
sommelier tail $since 2>/dev/null | while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^#\([0-9][0-9]*\) .*/\1/p')
  [ -z "$id" ] && continue
  echo "$id" > "$STATE"
  case "$line" in
    # Clients print the addressees as `to:a,b` or as `@a @b`, depending on their version.
    *" to:$ME "*|*" to:$ME,"*|*",$ME "*|*",$ME,"*|*" @$ME "*) ;;
    *) continue ;;
  esac
  echo "wake: #$id for $ME, starting the command" >&2
  printf '%s\n' "$line" | "$@"
  echo "wake: #$id done (exit $?)" >&2
done
