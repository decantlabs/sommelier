#!/bin/sh
# Launcher hook for Antigravity (Gemini / agy) workers (section 5.4).
#
# Environment provided by sommelier agent:
#   SOMMELIER_TASK, SOMMELIER_ATTEMPT   task id and attempt number
#   SOMMELIER_BRANCH, SOMMELIER_WORKTREE branch and worktree path (already made)
#   SOMMELIER_PROMPT                 assembled prompt file (guide, brief, resume note)
#   SOMMELIER_MODEL, SOMMELIER_EFFORT   model and effort chosen from class
#   SOMMELIER_OUT                    folder for last.txt, events.jsonl, err.txt
#
# Runs the agent in the foreground, writes the last message to $SOMMELIER_OUT/last.txt,
# passes the exit code, starts nothing in the background, and prints no secret.
set -u

for v in SOMMELIER_TASK SOMMELIER_ATTEMPT SOMMELIER_BRANCH SOMMELIER_WORKTREE SOMMELIER_PROMPT SOMMELIER_OUT; do
  eval "_val=\${$v:-}"
  if [ -z "$_val" ]; then
    echo "hook-agy.sh: missing required environment variable $v" >&2
    exit 2
  fi
done

unset SOMMELIER_TOKEN SOMMELIER_TOKEN_FILE SOMMELIER_ORCH_TOKEN
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

mkdir -p "$SOMMELIER_OUT" || exit 1
cd "$SOMMELIER_WORKTREE" || exit 1

AGY_BIN="${SOMMELIER_AGY:-agy}"
MODEL="${SOMMELIER_MODEL:-gemini-4-hydrogen-medium}"

set -- --print "$(cat "$SOMMELIER_PROMPT")" --model "$MODEL"
if [ -n "${SOMMELIER_EFFORT:-}" ]; then
  case "$MODEL" in
    *-low|*-medium|*-high|*-xhigh|gemini-pro-orchestrator*) ;;
    *) set -- "$@" --effort "$SOMMELIER_EFFORT" ;;
  esac
fi
set -- "$@" --dangerously-skip-permissions --output-format text

"$AGY_BIN" "$@" > "$SOMMELIER_OUT/last.txt" 2> "$SOMMELIER_OUT/err.txt"
exit $?
