#!/usr/bin/env bash
# Start one Claude Code agent. On the bus it shows as a worker "claude-<name>" of the orchestrator.
#
#   claude-agent.sh <worker name> <worktree folder> <branch> <prompt file> [model] [effort]
#
# The worktree ($SOMMELIER_REPOS/<folder>) is made from the shared main if it does not exist. The agent runs without permission
# prompts (allowed by the project owner). Its answer is written to $SOMMELIER_RUNS/claude/<worker>.last.txt.
# SOMMELIER_CLAUDE names the claude program (default: claude on the PATH).
set -u
if [ $# -lt 4 ]; then sed -n '2,8p' "$0"; exit 2; fi
NAME=$1; FOLDER=$2; BRANCH=$3; PROMPT=$4; MODEL=${5:-}; EFFORT=${6:-}
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
CLAUDE_BIN=${SOMMELIER_CLAUDE:-claude}
HOST_NAME=${SOMMELIER_MACHINE:-$(hostname 2>/dev/null || echo local)}
WT=$SOMMELIER_REPOS/$FOLDER
OUT=$SOMMELIER_RUNS/claude; mkdir -p "$OUT"
# The agent itself gets no bus token: like our other workers it reports through its branch and
# its last message. Its row on the dashboard is written here, as a worker of the orchestrator.
BUS() { [ -n "$SOMMELIER_URL" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" sommelier "$@"; }
ROSTER=$SOMMELIER_ROSTER
[ -d "$WT" ] || sh "$HERE/wt.sh" "$FOLDER" "$BRANCH" || exit 1
say() {
  local st=$1 txt=$2 pid_arg=${3:-}
  { grep -v -P "^claude-$NAME\t" "$ROSTER" 2>/dev/null; printf '%s\t%s\t%s\t%s\t%s\n' "claude-$NAME" "$st" "${MODEL:-claude}" "$BRANCH" "$txt"; } > "$ROSTER.claude.$$.${BASHPID:-0}" && mv "$ROSTER.claude.$$.${BASHPID:-0}" "$ROSTER"
  local extra=(--host "$HOST_NAME")
  [ -n "$pid_arg" ] && extra+=(--pid "$pid_arg")
  BUS status set "claude-$NAME" --state "$st" --model "${MODEL:-claude}" --branch "$BRANCH" "${extra[@]}" "$txt" >/dev/null 2>&1
}
TASK_DESC=$(grep -m1 '^TASK:' "$PROMPT" | sed 's/^TASK: *//' | head -c 160)
FINISHED=0
HB_PID=
CHILD_PID=
cleanup() {
  kill "${HB_PID:-}" 2>/dev/null || true
  rm -f "$OUT/$NAME.pid"
  if [ "$FINISHED" -eq 0 ]; then
    say idle "stopped unexpectedly"
  fi
}
trap cleanup EXIT
cd "$WT" || exit 1
args=(-p --dangerously-skip-permissions --output-format text)
[ -n "$MODEL" ] && [ "$MODEL" != "claude" ] && args+=(--model "$MODEL")
[ -n "$EFFORT" ] && args+=(--effort "$EFFORT")
(
  unset SOMMELIER_TOKEN SOMMELIER_TOKEN_FILE SOMMELIER_ORCH_TOKEN
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
  exec "$CLAUDE_BIN" "${args[@]}" < "$PROMPT" > "$OUT/$NAME.last.txt" 2> "$OUT/$NAME.err.txt"
) &
CHILD_PID=$!
echo "$CHILD_PID" > "$OUT/$NAME.pid"
say running "$TASK_DESC" "$CHILD_PID"
(
  while sleep 45; do
    kill -0 "$CHILD_PID" 2>/dev/null || exit 0
    say running "$TASK_DESC" "$CHILD_PID"
  done
) &
HB_PID=$!
wait "$CHILD_PID"
code=$?
FINISHED=1
kill "${HB_PID:-}" 2>/dev/null || true
wait "${HB_PID:-}" 2>/dev/null || true
echo "$code $(grep -a -m1 -E "READY|STUCK" "$OUT/$NAME.last.txt" 2>/dev/null | head -c 300)" > "$OUT/$NAME.done"
say idle "finished (exit $code): $(tail -c 300 "$OUT/$NAME.last.txt" 2>/dev/null | grep -a -m1 -E "READY|STUCK" | head -c 140)"
echo "claude agent $NAME ended with $code, answer in $OUT/$NAME.last.txt"
