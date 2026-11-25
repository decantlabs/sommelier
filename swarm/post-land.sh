#!/bin/sh
# Post-land hook: records metrics for a run folder (or runs run-title.sh on a worktree) and checks for regressions.
# Usage: post-land.sh [<run_folder_or_worktree>]
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
target=${1:-${SOMMELIER_INTEGRATE:-}}
[ -n "$target" ] || exit 0

if [ -d "$target" ] && { [ -r "$target/run.txt" ] || [ -r "$target/title.log" ]; }; then
  sh "$HERE/metrics.sh" "$target" || exit $?
  sh "$HERE/metrics-compare.sh"
elif [ -d "$target" ]; then
  sha=$(git -C "$target" rev-parse --short HEAD 2>/dev/null || echo main)
  out=$(sh "$HERE/run-title.sh" "$target" "${POST_LAND_SECS:-60}" "post-land-$sha") || exit $?
  folder=$(printf '%s\n' "$out" | sed -n 's/^run folder: //p')
  [ -n "$folder" ] && [ -d "$folder" ] && sh "$HERE/metrics.sh" "$folder"
  sh "$HERE/metrics-compare.sh"
fi
