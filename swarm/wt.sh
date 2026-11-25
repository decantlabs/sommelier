#!/bin/sh
# Makes a fresh worktree from the shared main.   usage: wt.sh <folder name under SOMMELIER_REPOS> <new branch>
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
cd "$SOMMELIER_CLONE" && git fetch -q "$SOMMELIER_UPSTREAM" && git worktree add "$SOMMELIER_REPOS/$1" -b "$2" "$SOMMELIER_UPSTREAM/main" && echo "worktree $SOMMELIER_REPOS/$1 on $2"
