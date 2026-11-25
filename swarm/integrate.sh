#!/bin/sh
# Lands branches of the fork on the shared main: rebase, repository check, workspace tests, clippy, push.
# usage: integrate.sh <branch> [<branch> ...]     (branch names as on SOMMELIER_FORK)
# One run at a time (lock). Prints RESULT lines. Full log: $SOMMELIER_STATE/logs/integrate-<time>.log
# The checkout $SOMMELIER_INTEGRATE is used only for this and must be clean. Remotes: SOMMELIER_UPSTREAM (the
# shared repository, receives the push) and SOMMELIER_FORK (where the branches are).
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
WT=$SOMMELIER_INTEGRATE
LOCK=$SOMMELIER_STATE/.integrate.lock
mkdir -p "$SOMMELIER_STATE/logs"
LOG=$SOMMELIER_STATE/logs/integrate-$(date +%m%d-%H%M%S).log
until mkdir "$LOCK" 2>/dev/null; do sleep 5; done
trap 'rmdir "$LOCK"' EXIT
export CARGO_BUILD_JOBS=12
cd "$WT" || exit 2
git rebase --abort >/dev/null 2>&1
git fetch -q "$SOMMELIER_UPSTREAM"; git fetch -q "$SOMMELIER_FORK"
git checkout -q --detach "$SOMMELIER_UPSTREAM/main"
tip=$SOMMELIER_UPSTREAM/main
landed=""
for b in "$@"; do
  git checkout -q -B "integ-tmp" "$SOMMELIER_FORK/$b" 2>>"$LOG" || { echo "RESULT $b MISSING on $SOMMELIER_FORK"; continue; }
  if git rebase "$tip" >>"$LOG" 2>&1; then
    tip=$(git rev-parse HEAD); landed="$landed $b"
  else
    echo "RESULT $b CONFLICT in: $(git diff --name-only --diff-filter=U | tr '\n' ' ')"
    git rebase --abort >/dev/null 2>&1
  fi
done
[ -z "$landed" ] && { echo "RESULT nothing to land"; exit 1; }
git checkout -q --detach "$tip"
if [ "$(git rev-parse HEAD)" = "$(git rev-parse "$SOMMELIER_UPSTREAM/main")" ]; then echo "RESULT already on main:$landed"; exit 0; fi
step() { name=$1; shift; echo "== $name" >>"$LOG"; if "$@" >>"$LOG" 2>&1; then return 0; fi
  echo "RESULT FAILED at $name for:$landed (log $LOG)"; grep -E "^(error|test .* FAILED|failures:|---- .* stdout)|panicked at|warning: unused|error\[" "$LOG" | tail -25; exit 1; }
step repo-check cargo run --locked -q -p decant-repo-check --bin check_repository
step test cargo test --workspace --locked
step clippy cargo clippy --workspace --all-targets --locked -- -D warnings
cd "$WT"
try=0
until git push -q "$SOMMELIER_UPSTREAM" HEAD:main >>"$LOG" 2>&1; do
  try=$((try+1)); [ $try -gt 3 ] && { echo "RESULT PUSH FAILED for:$landed (log $LOG)"; exit 1; }
  git fetch -q "$SOMMELIER_UPSTREAM"
  git rebase "$SOMMELIER_UPSTREAM/main" >>"$LOG" 2>&1 || { git rebase --abort; echo "RESULT CONFLICT with new main for:$landed"; exit 1; }
  ( cargo test --workspace --locked >>"$LOG" 2>&1 ) || { echo "RESULT FAILED after new main for:$landed (log $LOG)"; exit 1; }
done
echo "RESULT LANDED$landed as $(git rev-parse --short HEAD) on main"
