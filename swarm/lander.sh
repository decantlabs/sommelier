#!/bin/sh
# Lands queued branches on main, one batch after the other, without anyone starting it.
#
#   lander.sh                 runs forever
#   LANDER_DRY=1 lander.sh    prints synthetic game run decisions and exits
#   LANDER_GAME_RUN=0         skips the game run before pushing
#   Queue: $SOMMELIER_RUNS/landing.queue, one ref per line as git knows it in the landing checkout
#          ($SOMMELIER_INTEGRATE), for example origin/feature/some-fix or upstream/guest/some-fix
#          (a guest's branch). Append a line to queue a branch: echo origin/foo >> $SOMMELIER_RUNS/landing.queue
#
#          A line can also be a bundle entry of the public factory:
#          bundle:<sha256> <head sha> [member_handle=.. member_id=.. order=.. submission=.. email=..] [title=<rest of line>]
#          The lander downloads the bundle, checks it, and lands the submission as ONE commit with credit trailers.
#   SOMMELIER_SECOND_LOCK=1   pushes the checked batch as landing/<batch id> and pushes main only after GitHub
#                             reports success for the check SOMMELIER_BATCH_CHECK (see swarm/README.md)
#
# A batch: every queued branch is rebased onto the current main in the landing worktree (a branch
# that conflicts is left out and reported), then the repository check, the workspace tests and
# clippy run on that exact tree, then it is pushed to main and announced on the bus as LANDED.
# The GPU tests hang while a game run holds the GPU, so the batch takes the game lock only for the
# tests of the crates that need the GPU; the checks before it and all other tests run without it.
# A branch that changes files which steer agents, builds or this harness (GUARD) is HELD until
# the coordinator has read its diff and approved that commit in $SOMMELIER_RUNS/landing.approved.
# The landing lock ($SOMMELIER_STATE/.integrate.lock, shared with integrate.sh) carries its owner pid and
# is taken over from a dead lander.
# A batch that fails its checks is split by algorithm 4.4 so good branches land and only the bad
# branch is moved to landing.failed and reported: fix, then queue again.
# Every result is written to $SOMMELIER_RUNS/watchdog.findings, which wakes the coordinator.

HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lander-lib.sh"

if [ "${LANDER_DRY:-0}" = 1 ]; then
  src=crates/decant-xmem/src/lib.rs
  printf 'Docs and workflows: '; LANDER_GAME_RUN=1 game_run_decision "$(printf '%s\n' .github/workflows/example.yml docs/example.md)"
  printf 'Clean run: '; LANDER_GAME_RUN=1 game_run_decision "$src" result 3<<'LOG' 4</dev/null
Title running
LOG
  printf 'Removing Device: '; LANDER_GAME_RUN=1 game_run_decision "$src" result 3<<'LOG' 4</dev/null
Removing Device
LOG
  printf 'Title ended at 29 seconds: '; LANDER_GAME_RUN=1 game_run_decision "$src" result 3</dev/null 4<<'RUN'
ended by itself after 29 s
RUN
  printf 'Disabled: '; LANDER_GAME_RUN=0 game_run_decision "$src"
  printf 'UNHANDLED: '; LANDER_GAME_RUN=1 game_run_decision "$src" result 3<<'LOG' 4</dev/null
UNHANDLED
LOG
  printf 'Title ended at 55 seconds: '; LANDER_GAME_RUN=1 game_run_decision "$src" result 3</dev/null 4<<'RUN'
ended by itself after 55 s
RUN
  printf 'Missing run folder: '; LANDER_GAME_RUN=1 game_run_decision "$src" missing
  LANDER_GPU_TOTAL=1500
  GPU_DRY_LIST="gpu-fast-a gpu-slow-b gpu-fast-c"
  GPU_DRY_ELAPSED=0
  GPU_DRY_NEXT=1200
  printf 'GPU total deadline after %s seconds: ' "$LANDER_GPU_TOTAL"
  for c in $GPU_DRY_LIST; do
    if [ "$GPU_DRY_ELAPSED" -ge "$LANDER_GPU_TOTAL" ]; then
      printf 'fail (overall limit; not yet tested: %s)\n' "$GPU_DRY_NEXT"
      break
    fi
    printf '%s tested; ' "$c"
    case $c in
      gpu-slow-b)
        GPU_DRY_ELAPSED=$((GPU_DRY_ELAPSED + 1600))
        printf 'fail (overall limit; not yet tested: gpu-fast-c)\n'
        break ;;
      *) GPU_DRY_ELAPSED=$((GPU_DRY_ELAPSED + 60)); GPU_DRY_NEXT="gpu-slow-b gpu-fast-c" ;;
    esac
  done
  _dry_hash=0000000000000000000000000000000000000000000000000000000000000000
  _dry_entry="bundle:$_dry_hash 1111111111111111111111111111111111111111 member_handle=ada member_id=m1 order=t412 submission=$_dry_hash title=Fix the thing"
  printf 'Bundle entry: download <hub>/api/bundle/<sha256>, check the hash, fetch into refs/submissions/<sha256>, check the head, squash onto the batch base with this message:\n'
  lander_commit_message "$_dry_entry" | sed 's/^/  | /'
  if [ "${SOMMELIER_SECOND_LOCK:-0}" = 1 ]; then
    printf 'Second lock: push landing/<batch id>, ask GitHub for the check %s every %s s for up to %s s, push main only on success, delete the landing branch\n' "${SOMMELIER_BATCH_CHECK:-factory-batch}" "${SOMMELIER_BATCH_CHECK_PAUSE:-15}" "${SOMMELIER_BATCH_CHECK_SECS:-1800}"
  else
    printf 'Second lock: off (SOMMELIER_SECOND_LOCK is not 1), main is pushed as before\n'
  fi
  exit 0
fi

. "$HERE/env.sh"
WT=$SOMMELIER_INTEGRATE
SW=$SOMMELIER_STATE
RUNS=$SOMMELIER_RUNS; Q=$RUNS/landing.queue; FIND=$RUNS/watchdog.findings
mkdir -p "$SW/logs"
touch "$Q" "$FIND"

while :; do
  if [ -d "$SW/.integrate.lock" ]; then
    owner=$(cat "$SW/.integrate.lock/pid" 2>/dev/null)
    if [ -z "$owner" ] || ! kill -0 "$owner" 2>/dev/null; then
      age=$(( $(date +%s) - $(stat -c %Y "$SW/.integrate.lock") ))
      [ -n "$owner" ] || [ "$age" -gt 3600 ] && { rm -rf "$SW/.integrate.lock"; say "removed a landing lock whose owner is gone"; }
    fi
  fi
  if [ -s "$Q" ] && mkdir "$SW/.integrate.lock" 2>/dev/null; then
    echo $$ > "$SW/.integrate.lock/pid"
    _batch_id="batch-$(date +%s)"
    _find_before=$(wc -l < "$FIND" 2>/dev/null || echo 0)
    _failed_before=$(wc -l < "$RUNS/landing.failed" 2>/dev/null || echo 0)
    _conflict_before=$(wc -l < "$RUNS/landing.conflict" 2>/dev/null || echo 0)
    _held_before=$(wc -l < "$RUNS/landing.held" 2>/dev/null || echo 0)
    if batch; then
      if [ -n "${SOMMELIER_URL:-}" ]; then
        _find_after=$(wc -l < "$FIND" 2>/dev/null || echo 0)
        if [ "$_find_after" -gt "$_find_before" ]; then
          _landed_refs=$(tail -n +"$((_find_before + 1))" "$FIND" 2>/dev/null | sed -n 's/.*LANDED as [^ ]* on main: //p' | tr '\n' ' ')
          for _r in $_landed_refs; do
            _r_sha=$(awk -v r="$_r" '$1 == r { s = $2 } END { print s }' "$Q.seen" 2>/dev/null)
            [ -z "$_r_sha" ] && _r_sha=$(git -C "$WT" rev-parse "$_r" 2>/dev/null || echo "")
            [ -n "$_r_sha" ] && BUS land result "$_batch_id" --result landed --ref "$_r" --sha "$_r_sha" || true
          done
        fi
        _failed_after=$(wc -l < "$RUNS/landing.failed" 2>/dev/null || echo 0)
        if [ "$_failed_after" -gt "$_failed_before" ]; then
          for _r in $(tail -n +"$((_failed_before + 1))" "$RUNS/landing.failed" 2>/dev/null); do
            _r_sha=$(awk -v r="$_r" '$1 == r { s = $2 } END { print s }' "$Q.seen" 2>/dev/null)
            [ -z "$_r_sha" ] && _r_sha=$(git -C "$WT" rev-parse "$_r" 2>/dev/null || echo "")
            [ -n "$_r_sha" ] && BUS land result "$_batch_id" --result failed --ref "$_r" --sha "$_r_sha" || true
          done
        fi
        _conflict_after=$(wc -l < "$RUNS/landing.conflict" 2>/dev/null || echo 0)
        if [ "$_conflict_after" -gt "$_conflict_before" ]; then
          for _r in $(tail -n +"$((_conflict_before + 1))" "$RUNS/landing.conflict" 2>/dev/null); do
            _r_sha=$(awk -v r="$_r" '$1 == r { s = $2 } END { print s }' "$Q.seen" 2>/dev/null)
            [ -z "$_r_sha" ] && _r_sha=$(git -C "$WT" rev-parse "$_r" 2>/dev/null || echo "")
            [ -n "$_r_sha" ] && BUS land result "$_batch_id" --result conflict --ref "$_r" --sha "$_r_sha" || true
          done
        fi
        _held_after=$(wc -l < "$RUNS/landing.held" 2>/dev/null || echo 0)
        if [ "$_held_after" -gt "$_held_before" ]; then
          tail -n +"$((_held_before + 1))" "$RUNS/landing.held" 2>/dev/null | while read -r _r _r_sha; do
            _full_sha=$(awk -v r="$_r" '$1 == r { s = $2 } END { print s }' "$Q.seen" 2>/dev/null)
            [ -z "$_full_sha" ] && _full_sha=$_r_sha
            [ -n "$_r" ] && [ -n "$_full_sha" ] && BUS land result "$_batch_id" --result held --ref "$_r" --sha "$_full_sha" || true
          done
        fi
      fi
    fi
    rm -rf "$SW/.integrate.lock"
  fi
  sleep 30
done
