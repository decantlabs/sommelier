#!/bin/sh
# Launcher hook for Codex workers (section 5.4).
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
    echo "hook-codex.sh: missing required environment variable $v" >&2
    exit 2
  fi
done

unset SOMMELIER_TOKEN SOMMELIER_TOKEN_FILE SOMMELIER_ORCH_TOKEN
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

mkdir -p "$SOMMELIER_OUT" || exit 1
cd "$SOMMELIER_WORKTREE" || exit 1

to_win() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

CODEX_BIN="${SOMMELIER_CODEX:-codex}"
WT_WIN=$(to_win "$SOMMELIER_WORKTREE")
LAST_WIN=$(to_win "$SOMMELIER_OUT/last.txt")

set -- exec -C "$WT_WIN" --dangerously-bypass-approvals-and-sandbox --json -o "$LAST_WIN"
if [ -n "${SOMMELIER_MODEL:-}" ] && [ "$SOMMELIER_MODEL" != "codex" ]; then
  set -- "$@" -m "$SOMMELIER_MODEL"
fi
if [ -n "${SOMMELIER_EFFORT:-}" ]; then
  set -- "$@" -c "model_reasoning_effort=\"$SOMMELIER_EFFORT\""
fi

"$CODEX_BIN" "$@" - < "$SOMMELIER_PROMPT" > "$SOMMELIER_OUT/events.jsonl" 2> "$SOMMELIER_OUT/err.txt"
exit $?
