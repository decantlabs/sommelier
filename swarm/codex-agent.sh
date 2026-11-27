#!/usr/bin/env bash
# Start one Codex agent. On the bus it shows as a worker "codex-<name>" of the orchestrator.
#
#   codex-agent.sh <worker name> <worktree folder> <branch> <prompt file> [model]
#
# The worktree ($SOMMELIER_REPOS/<folder>) is made from the shared main if it does not exist. The agent's status row on the bus
# is set to running at the start and to idle at the end, whatever the agent itself does. Its last
# message is written to $SOMMELIER_RUNS/codex/<worker>.last.txt, the full event log beside it.
set -u
if [ $# -lt 4 ]; then sed -n '2,8p' "$0"; exit 2; fi
NAME=$1; FOLDER=$2; BRANCH=$3; PROMPT=$4; MODEL=${5:-}
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
HOST_NAME=${SOMMELIER_MACHINE:-$(hostname 2>/dev/null || echo local)}
WT=$SOMMELIER_REPOS/$FOLDER
OUT=$SOMMELIER_RUNS/codex; mkdir -p "$OUT"
# The agent itself gets no bus token: like our other workers it reports through its branch and
# its last message. Its row on the dashboard is written here, as a worker of the orchestrator.
BUS() { [ -n "$SOMMELIER_URL" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" sommelier "$@"; }
ROSTER=$SOMMELIER_ROSTER
export CARGO_BUILD_JOBS=4
[ -d "$WT" ] || sh "$HERE/wt.sh" "$FOLDER" "$BRANCH" || exit 1
say() {  # state, text, [pid]: one row in the orchestrator's roster file and on the bus
  local st=$1 txt=$2 pid_arg=${3:-}
  { grep -v -P "^codex-$NAME\t" "$ROSTER" 2>/dev/null; printf '%s\t%s\t%s\t%s\t%s\n' "codex-$NAME" "$st" "${MODEL:-codex}" "$BRANCH" "$txt"; } > "$ROSTER.codex.$$" && mv "$ROSTER.codex.$$" "$ROSTER"
  local extra=(--host "$HOST_NAME")
  [ -n "$pid_arg" ] && extra+=(--pid "$pid_arg")
  BUS status set "codex-$NAME" --state "$st" --model "${MODEL:-codex}" --branch "$BRANCH" "${extra[@]}" "$txt" >/dev/null 2>&1
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
# Sandbox: the agent may write in its worktree, the cargo and compiler cache folders and the run
# folder, and may use the network (git push, the bus). CODEX_FULL=1 drops the sandbox altogether:
# the project owner allowed it on 24 Nov 2026 (CODEX_FULL=0 turns the sandbox back on).
if [ "${CODEX_FULL:-1}" = 1 ]; then
  box=(--dangerously-bypass-approvals-and-sandbox)
else
  box=(-s workspace-write -c sandbox_workspace_write.network_access=true
       --add-dir "$(cygpath -w "$HOME/.cargo")" --add-dir "$(cygpath -w "${LOCALAPPDATA:-$HOME/.cache}/Mozilla/sccache")"
       --add-dir "$(cygpath -w "$SOMMELIER_CLONE/.git")" --add-dir "$(cygpath -w "$SOMMELIER_RUNS")")
fi
args=(exec -C "$(cygpath -w "$WT")" "${box[@]}" --json -o "$(cygpath -w "$OUT/$NAME.last.txt")")
[ -n "$MODEL" ] && args+=(-m "$MODEL")
codex "${args[@]}" - < "$PROMPT" > "$OUT/$NAME.events.jsonl" 2> "$OUT/$NAME.err.txt" &
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
say idle "finished (exit $code): $(head -c 120 "$OUT/$NAME.last.txt" 2>/dev/null | tr '\n' ' ')"
echo "codex agent $NAME ended with $code, last message in $OUT/$NAME.last.txt"
