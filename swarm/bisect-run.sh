#!/bin/sh
# git bisect run helper: builds/runs the title at HEAD, records metrics, and exits 1 on REGRESSION.
# Usage: git bisect run sh swarm/bisect-run.sh <good_sha> [<worktree>]
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
GOOD_SHA=${1:-}
if [ -z "$GOOD_SHA" ]; then
  echo "usage: $(basename "$0") <good_sha> [<worktree>]" >&2
  exit 125
fi

WT=${2:-$(pwd)}
CUR_SHA=$(git -C "$WT" rev-parse --short HEAD 2>/dev/null || echo "")
[ -n "$CUR_SHA" ] || exit 125

out=$(sh "$HERE/run-title.sh" "$WT" "${BISECT_SECS:-60}" "bisect-$CUR_SHA") || exit 125
folder=$(printf '%s\n' "$out" | sed -n 's/^run folder: //p')
[ -n "$folder" ] && [ -d "$folder" ] || exit 125

sh "$HERE/metrics.sh" "$folder" || exit 125
cmp=$(sh "$HERE/metrics-compare.sh" "$CUR_SHA" "$GOOD_SHA")
if printf '%s\n' "$cmp" | grep -q '^REGRESSION'; then
  printf '%s\n' "$cmp"
  exit 1
fi
exit 0
