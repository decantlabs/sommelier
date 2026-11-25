#!/usr/bin/env bash
# One run of the title on this PC, queued behind any other run.
#
#   run-title.sh <worktree> <seconds> <label> [VAR=value ...]
#
# <worktree>  a checkout of the project. The title is started from the executable named by
#             SOMMELIER_TITLE_EXE. A relative SOMMELIER_TITLE_EXE is taken inside the worktree.
# <seconds>   how long the title runs before it is stopped.
# <label>     a short name for the run folder.
# VAR=value   environment for the title, on top of the current environment. SHOTS="60 80 100"
#             takes window screenshots at those seconds too (window-<second>.png), besides the
#             one shortly before the end. GENTLE=auto|1|0 sets gentle mode (see below).
#
# Runs never overlap: the script waits for the lock folder $SOMMELIER_RUNS/.lock. Everything of the
# run (log, screenshots, summary) is copied to $SOMMELIER_RUNS/<time>-<label>/, and that path and a
# few numbers are printed at the end.
#
# Settings (swarm/env.sh):
#   SOMMELIER_TITLE_EXE      the executable of the title (required)
#   SOMMELIER_TITLE_ARGS     arguments for it (optional, split on spaces)
#   SOMMELIER_TITLE_DIR      working folder of the title, default: the folder of the executable
#   SOMMELIER_TITLE_PROCESS  image name of the process, default: the file name of the executable
#   SOMMELIER_TITLE_LOG      a log file the title writes, copied to title.log (optional, the console
#                         output becomes title.log when it is empty). A relative path is taken
#                         inside the worktree.
#   SOMMELIER_RUNS           where the lock, the queue and the run folders live
set -u
if [ $# -lt 3 ]; then sed -n '2,25p' "$0"; exit 2; fi
worktree=$1; SECS=$2; LABEL=$3; shift 3
HERE=$(cd "$(dirname "$0")" && pwd)
for kv in "$@"; do
  case "$kv" in
    *=*) ;;
    *) printf 'run-title: not VAR=value: %s\n' "$kv" >&2; exit 2 ;;
  esac
done
WT=$(cygpath -m "$worktree" 2>/dev/null || printf '%s' "$worktree")
. "$HERE/env.sh"
sommelier_need SOMMELIER_TITLE_EXE "the executable of the title"
RUNS=$SOMMELIER_RUNS; LOCK=$RUNS/.lock
mkdir -p "$RUNS"

EXE=$SOMMELIER_TITLE_EXE
case "$EXE" in /*|[A-Za-z]:*) ;; *) EXE=$WT/$EXE ;; esac
[ -f "$EXE" ] || { echo "run-title: no $EXE (build the title first)"; exit 2; }
TITLE_PROC=${SOMMELIER_TITLE_PROCESS:-$(basename "$EXE")}
TITLE_DIR=${SOMMELIER_TITLE_DIR:-$(dirname "$EXE")}
TITLE_LOG=${SOMMELIER_TITLE_LOG:-}
case "$TITLE_LOG" in ''|/*|[A-Za-z]:*) ;; *) TITLE_LOG=$WT/$TITLE_LOG ;; esac

SHOTS=
# Gentle mode (title at below normal CPU and GPU priority, SOMMELIER_GENTLE for the title). GENTLE=auto (default) decides
# just before the title starts: on when cargo, rustc, link, dxc or clippy run or a build worker of the Gemini launcher
# runs, else off. GENTLE=1 and GENTLE=0 force it (give GENTLE=1 to a run that only reads something out).
GENTLE=${GENTLE:-auto}; unset SOMMELIER_GENTLE
for kv in "$@"; do
  case "$kv" in
    SHOTS=*) SHOTS="${kv#SHOTS=}" ;;
    GENTLE=*|SOMMELIER_GENTLE=*) GENTLE=${kv#*=} ;;
    *=*) export "$kv" ;;
  esac
done
# Window screenshots at least every 30 s unless SHOTS says otherwise.
[ -z "$SHOTS" ] && SHOTS=$(seq 30 30 $((SECS > 40 ? SECS - 10 : 30)) | tr '\n' ' ')

stop_title() { taskkill //F //IM "$TITLE_PROC" >/dev/null 2>&1; }

# The owner of the PC can hold all runs: while the file $SOMMELIER_RUNS/.pause exists no run starts
# (a run that is already going finishes). In gentle mode (see GENTLE above) the title runs at
# below normal CPU priority so the desktop stays usable (boot is slower then, say so in reports).
# A ticket per waiting run, so the watchdog can see who waits and how long.
mkdir -p "$RUNS/.queue"; TICKET=$RUNS/.queue/$$; echo "$LABEL $(date +%s) $SECS" > "$TICKET"; trap 'rm -f "$TICKET"' EXIT
paused=0
while [ -e "$RUNS/.pause" ]; do
  [ $paused -eq 0 ] && echo "run-title: paused by $RUNS/.pause, waiting"
  paused=1; sleep 10
done

waited=0
until mkdir "$LOCK" 2>/dev/null; do
  # A lock whose owner is gone (its shell died) is taken over.
  owner=$(cat "$LOCK/pid" 2>/dev/null || echo)
  if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then echo "run-title: stale lock of process $owner removed"; rm -rf "$LOCK"; continue; fi
  [ $((waited % 60)) -eq 0 ] && echo "run-title: waiting for the run of $(cat "$LOCK/label" 2>/dev/null) ($waited s)"
  sleep 5; waited=$((waited + 5))
done
echo $$ > "$LOCK/pid"; echo "$LABEL" > "$LOCK/label"
echo "$SECS" > "$LOCK/secs"; rm -f "$TICKET"
trap 'stop_title; rm -rf "$LOCK"' EXIT
trap 'exit 130' INT TERM

OUT=$RUNS/$(date +%Y%m%d-%H%M%S)-$LABEL
mkdir -p "$OUT"
SOMMELIER_RUN_ID=$(basename "$OUT")
SOMMELIER_RUN_SHA=$(git -C "$WT" rev-parse HEAD 2>/dev/null || echo "")
if [ -n "${SOMMELIER_URL:-}" ] && command -v "${SOMMELIER_BIN:-sommelier}" >/dev/null 2>&1; then
  SOMMELIER_TOKEN_FILE="${SOMMELIER_ORCH_TOKEN:-${SOMMELIER_TOKEN_FILE:-}}" timeout 15 "${SOMMELIER_BIN:-sommelier}" run register --run-id "$SOMMELIER_RUN_ID" --sha "$SOMMELIER_RUN_SHA" --label "$LABEL" >/dev/null 2>&1 || true
fi
stop_title
# The process of the run before may still hold files for a moment.
for _ in 1 2 3 4 5 6 7 8 9 10; do
  tasklist 2>/dev/null | grep -qi -F "$TITLE_PROC" || break
  sleep 1
done
sleep 3
GENTLE_WHY="forced by GENTLE=$GENTLE"
case "$GENTLE" in
  1) export SOMMELIER_GENTLE=1 ;;
  0) export SOMMELIER_GENTLE=0 ;;
  *)
    busy=$(tasklist 2>/dev/null | grep -a -i -o -E '^(cargo|rustc|link|dxc|clippy-driver|cargo-clippy)\.exe' | sort -u | tr '\n' ' ')
    GENTLE_WHY=; [ -n "$busy" ] && GENTLE_WHY="running: $busy"
    for sf in $SOMMELIER_RUNS/gemini/state/*; do
      [ -f "$sf" ] || continue
      read -r sst spid _ scls < "$sf" || true
      if [ "${sst:-}" = running ] && [ "${scls:-}" = build ] && kill -0 "$spid" 2>/dev/null; then GENTLE_WHY="$GENTLE_WHY build worker $(basename "$sf")"; fi
    done
    if [ -n "$GENTLE_WHY" ]; then export SOMMELIER_GENTLE=1; GENTLE_WHY="auto, on because $GENTLE_WHY"
    else export SOMMELIER_GENTLE=0; GENTLE_WHY="auto, off because no build tool or build worker runs"; fi ;;
esac
[ -n "$TITLE_LOG" ] && rm -f "$TITLE_LOG"
{
  echo "worktree $WT at $(git -C "$WT" log -1 --format='%h %s' 2>/dev/null)"
  echo "title $EXE"
  echo "seconds $SECS"
  env | grep -E '^SOMMELIER_' | grep -v -E 'TOKEN' | sort
  echo "gentle: SOMMELIER_GENTLE=${SOMMELIER_GENTLE:-unset} ($GENTLE_WHY)"
} > "$OUT/run.txt"

# shellcheck disable=SC2086
( cd "$TITLE_DIR" && exec "$EXE" ${SOMMELIER_TITLE_ARGS:-} ) > "$OUT/console.txt" 2>&1 &
start=$(date +%s)
shot_at=$((SECS > 8 ? SECS - 4 : SECS / 2))
shot_done=0
gentle_done=0
shot() {  # <png> <txt>
  timeout 25 powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$HERE/shot.ps1")" -Process "${TITLE_PROC%.exe}" -Out "$(cygpath -w "$1")" > "$2" 2>&1
}
while :; do
  now=$(( $(date +%s) - start ))
  if [ $gentle_done -eq 0 ] && [ $now -ge 3 ] && [ "${SOMMELIER_GENTLE:-0}" = 1 ]; then
    powershell -NoProfile -Command "Get-Process '${TITLE_PROC%.exe}' -ErrorAction SilentlyContinue | ForEach-Object { \$_.PriorityClass = 'BelowNormal' }" >/dev/null 2>&1
    echo "gentle: the PowerShell fallback lowered the CPU priority too (for titles without gentle mode)" >> "$OUT/run.txt"
    gentle_done=1
  fi
  if [ $shot_done -eq 0 ] && [ $now -ge $shot_at ]; then
    shot "$OUT/window.png" "$OUT/shot.txt"
    shot_done=1
  fi
  for at in $SHOTS; do
    if [ $now -ge "$at" ] && [ ! -f "$OUT/shot-$at.txt" ]; then
      shot "$OUT/window-$at.png" "$OUT/shot-$at.txt"
    fi
  done
  [ $now -ge "$SECS" ] && break
  # The title ended by itself (after its first 20 s, in which it may not be up yet).
  if [ $now -ge 20 ] && ! tasklist //FI "IMAGENAME eq $TITLE_PROC" 2>/dev/null | grep -qi -F "$TITLE_PROC"; then echo "ended by itself after $now s" >> "$OUT/run.txt"; break; fi
  sleep 1
done
stop_title
sleep 8   # the memory of the title goes back to the system before the next run starts
LOG=$OUT/title.log
if [ -n "$TITLE_LOG" ] && [ -f "$TITLE_LOG" ]; then cp "$TITLE_LOG" "$LOG" 2>/dev/null; else cp "$OUT/console.txt" "$LOG" 2>/dev/null; fi

last() { grep -a "$1" "$LOG" 2>/dev/null | tail -1; }
probe=$(last 'present probe: frame [0-9]* (thread')
{
  echo "run folder: $OUT"
  echo "frames: $(echo "$probe" | sed -n 's/.*present probe: frame \([0-9]*\).*/\1/p') at $(echo "$probe" | awk '{print $1}') s (last probe sample)"
  echo "device loss: $(grep -a -c -i -E 'DEVICE_REMOVED|DEVICE_HUNG|device removed|device is gone|device was removed|Removing Device' "$LOG" 2>/dev/null) lines"
  echo "unhandled: $(grep -a 'UNHANDLED' "$LOG" 2>/dev/null | head -2 | cut -c1-160 | tr '\n' '|')"
  echo "exit: $(grep -a ' exit [0-9]' "$LOG" 2>/dev/null | tail -1 | cut -c1-120)"
  echo "screenshot: $(tail -1 "$OUT/shot.txt" 2>/dev/null)"
  for at in $SHOTS; do echo "screenshot at $at s: $(tail -1 "$OUT/shot-$at.txt" 2>/dev/null)"; done
} | tee "$OUT/summary.txt"
SOMMELIER_RUNS="$RUNS" sh "$HERE/metrics.sh" "$OUT" >/dev/null 2>&1 || true
if [ -n "${SOMMELIER_URL:-}" ] && command -v "${SOMMELIER_BIN:-sommelier}" >/dev/null 2>&1; then
  _run_outcome=pass
  grep -aqE 'Removing Device|UNHANDLED' "$LOG" 2>/dev/null && _run_outcome=fail
  SOMMELIER_TOKEN_FILE="${SOMMELIER_ORCH_TOKEN:-${SOMMELIER_TOKEN_FILE:-}}" timeout 15 "${SOMMELIER_BIN:-sommelier}" run result "$SOMMELIER_RUN_ID" --outcome "$_run_outcome" --sha "$SOMMELIER_RUN_SHA" --label "$LABEL" >/dev/null 2>&1 || true
fi
