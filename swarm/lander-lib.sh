#!/bin/sh
# Shared library for lander.sh: game run gate, disk guard, licence scan,
# queue preparation, verification wrapper, and algorithm 4.4 batch splitting.

# With a result, fd 3 holds title.log and fd 4 holds run.txt.
game_run_decision() {
  if [ "${LANDER_GAME_RUN:-1}" = 0 ]; then
    echo "skip (LANDER_GAME_RUN=0)"
  elif ! printf '%s\n' "$1" | grep -qEv '^"?(docs/|\.github/)|\.md"?$|^$'; then
    echo "skip (only docs, workflows or Markdown changed)"
  else
    case ${2:-pending} in
      pending) echo run ;;
      missing) echo "refuse (missing run folder or logs)" ;;
      result)
        if grep -aqE 'Removing Device|UNHANDLED' <&3; then
          echo "refuse (Removing Device or UNHANDLED in title.log)"
        elif awk '$0 ~ /^ended by itself after [0-9]+ s[[:space:]]*$/ && $5 < 55 { early = 1 } END { exit !early }' <&4; then
          echo "refuse (title ended before 55 seconds)"
        else
          echo pass
        fi ;;
    esac
  fi
}

# Crates whose tests use the GPU, the game's memory or start processes: only these run under the
# game lock, so game runs wait for minutes less. Everything else is tested without the lock.
GPU_CRATES=${SOMMELIER_GPU_CRATES:-"decant-d3d12x decant-dxil decant-shader-ab decant-shader-harness decant-shader-bench decant-ffx decant-bvh decant-xmem decant-launch"}
GPU_EXCLUDE=""
GPU_ONLY=""
for _c in $GPU_CRATES; do
  GPU_EXCLUDE="$GPU_EXCLUDE --exclude $_c"
  GPU_ONLY="$GPU_ONLY -p $_c"
done
unset _c

# One deadline covers the whole GPU step, in addition to each crate's own limits.
LANDER_GPU_TOTAL=${LANDER_GPU_TOTAL:-1500}
LANDER_MAX_BATCH=${LANDER_MAX_BATCH:-8}
LANDER_INFRA_RETRIES=${LANDER_INFRA_RETRIES:-1}
LANDER_RETRY_WAIT_SEC=${LANDER_RETRY_WAIT_SEC:-120}
LANDER_MIN_FREE_GB=${LANDER_MIN_FREE_GB:-40}
LANDER_INFRA_PATTERNS=${LANDER_INFRA_PATTERNS:-"timed out|No space left|could not lock|failed to fetch|LNK1102"}

# Paths that steer agents (instruction files, skills), run on every machine (scripts, build steps,
# workflows), bring in outside code (the lock file) or are the checks themselves.
GUARD='(^|/)(\.github|\.claude)/|(^|/)(CLAUDE|AGENTS)\.md$|^(swarm|sommelier|bus)/|(^|/)build\.rs$|\.(sh|ps1|cmd|bat)$|(^|/)Cargo\.lock$|(^|/)decant-repo-check/'

say() {
  echo "LANDER $*" >> "$FIND"
  echo "$(date '+%F %T') $*" >> "$RUNS/lander.log"
}

BUS() {
  [ -n "${SOMMELIER_URL:-}" ] && SOMMELIER_TOKEN_FILE="${SOMMELIER_ORCH_TOKEN:-}" timeout 20 "${SOMMELIER_BIN:-sommelier}" "$@" >/dev/null 2>&1
}

lander_disk_free_gb() {
  if [ -n "${LANDER_DISK_FREE_GB:-}" ]; then
    printf '%s\n' "$LANDER_DISK_FREE_GB"
    return 0
  fi
  df -Pk "$WT" 2>/dev/null | awk 'NR==2 { printf "%d\n", int($4 / 1048576) }'
}

lander_disk_guard() {
  free_gb=$(lander_disk_free_gb)
  [ -z "$free_gb" ] && return 0
  if [ "$free_gb" -lt "$LANDER_MIN_FREE_GB" ]; then
    hook=${LANDER_CLEANUP_HOOK:-${HERE:-}/cleanup.sh}
    if [ -n "$hook" ] && [ -f "$hook" ]; then
      sh "$hook" >> "$RUNS/lander.log" 2>&1 || true
      free_gb=$(lander_disk_free_gb)
    fi
    if [ "$free_gb" -lt "$LANDER_MIN_FREE_GB" ]; then
      say "DISK_LOW: ${free_gb} GB free on $WT (need ${LANDER_MIN_FREE_GB} GB), landing paused"
      return 1
    fi
  fi
  return 0
}

lander_is_infra_failure() {
  [ -f "$1" ] || return 1
  grep -aqiE "$LANDER_INFRA_PATTERNS" "$1"
}

# Scan added lines of a branch diff for licence markers.
# Bracket expressions prevent this file from matching its own pattern.
lander_licence_marker_scan() {
  _base=$1
  _ref=$2
  _pat='([G]PL-[23]\.0|[L]GPL-[23]\.[01]|[A]GPL-3\.0|GNU [G]eneral Public License|GNU [L]esser General Public License|GNU [A]ffero General Public License|SPDX-License-Identifier:[[:space:]]*([G]PL|[L]GPL|[A]GPL))'
  if git diff -U0 "$_base...$_ref" 2>/dev/null | grep -E '^\+[^+]' | grep -qE "$_pat"; then
    return 1
  fi
  return 0
}

gpu_crate_tests() {
  gpu_started=$(date +%s)
  gpu_total_deadline=$((gpu_started + LANDER_GPU_TOTAL))
  gpu_remaining="$GPU_CRATES"
  for c in $GPU_CRATES; do
    gpu_now=$(date +%s)
    gpu_left=$((gpu_total_deadline - gpu_now))
    if [ "$gpu_left" -le 0 ]; then
      echo "GPU TOTAL LIMIT FAILED (${LANDER_GPU_TOTAL} s): not yet tested: $gpu_remaining"
      return 1
    fi
    gpu_remaining_after=${gpu_remaining#"$c"}
    gpu_remaining_after=${gpu_remaining_after# }
    echo "== gpu crate $c"
    case $c in decant-d3d12x|decant-dxil|decant-xmem) gpu_crate_limit=300 ;; *) gpu_crate_limit=60 ;; esac
    gpu_build_limit=600
    [ "$gpu_left" -lt "$gpu_build_limit" ] && gpu_build_limit=$gpu_left
    timeout "$gpu_build_limit" cargo test --locked --no-run -p "$c" || { echo "GPU CRATE BUILD FAILED OR TOTAL LIMIT EXPIRED: $c; not yet tested: $c $gpu_remaining_after"; return 1; }
    gpu_now=$(date +%s)
    gpu_left=$((gpu_total_deadline - gpu_now))
    if [ "$gpu_left" -le 0 ]; then
      echo "GPU TOTAL LIMIT FAILED (${LANDER_GPU_TOTAL} s): not yet tested: $c $gpu_remaining_after"
      return 1
    fi
    gpu_test_limit=$gpu_crate_limit
    [ "$gpu_left" -lt "$gpu_test_limit" ] && gpu_test_limit=$gpu_left
    timeout "$gpu_test_limit" cargo test --locked -p "$c" -- --test-threads=1 || { echo "GPU CRATE TESTS FAILED OR HUNG: $c; not yet tested: $gpu_remaining_after"; return 1; }
    gpu_remaining=$gpu_remaining_after
  done
}

game_run_check() (
  changed=$(git diff --name-only --no-renames "$base...$tip") || return 1
  decision=$(game_run_decision "$changed")
  if [ "$decision" != run ]; then
    echo "game-run: $decision"
    return 0
  fi

  echo "== game-run (60 seconds)"
  export CARGO_BUILD_JOBS=4
  # SOMMELIER_TITLE_BUILD is an optional shell command that builds the title and its runtime pieces
  # inside the worktree before the run.
  if [ -n "${SOMMELIER_TITLE_BUILD:-}" ]; then
    (cd "$WT" && sh -c "$SOMMELIER_TITLE_BUILD") || { echo "error: game-run could not build the title"; return 1; }
  fi

  unset SOMMELIER_KEYS
  runner=${LANDER_RUN_TITLE:-${HERE:-$(dirname "$0")}/run-title.sh}
  # SOMMELIER_TITLE_RUN_ARGS holds extra VAR=value words for the title (split on spaces on purpose).
  # shellcheck disable=SC2086
  report=$(sh "$runner" "$WT" 60 landing-check ${SOMMELIER_TITLE_RUN_ARGS:-})
  status=$?
  printf '%s\n' "$report"
  folder=$(printf '%s\n' "$report" | sed -n 's/^run folder: //p')
  if [ -n "$folder" ] && [ -n "${RUN_FOLDER_FILE:-}" ]; then
    printf '%s\n' "$folder" > "$RUN_FOLDER_FILE"
  fi
  if [ "$status" -ne 0 ] || [ ! -d "$folder" ] ||
     [ ! -r "$folder/title.log" ] || [ ! -r "$folder/run.txt" ]; then
    decision=$(game_run_decision "$changed" missing)
  else
    decision=$(game_run_decision "$changed" result 3<"$folder/title.log" 4<"$folder/run.txt") || return 1
  fi
  if [ "$decision" = pass ]; then
    echo "game-run: $decision"
  else
    echo "error: game-run $decision"
    return 1
  fi
)

lander_verify() {
  _refs=$1
  _vlog="$LOG.step"
  : > "$_vlog"
  LAST_RUN_FOLDER=""
  RUN_FOLDER_FILE="$LOG.runfolder"
  : > "$RUN_FOLDER_FILE"
  export RUN_FOLDER_FILE

  echo "== verify: $_refs" >> "$_vlog"
  # Everything that needs no GPU runs first, without the game lock: the repository check, the
  # test build and clippy. Only the test run itself holds the lock, so game runs wait minutes less.
  ( export CARGO_BUILD_JOBS=12 &&
    { echo "== repo-check" >>"$_vlog"; cargo run --locked -q -p decant-repo-check --bin check_repository >>"$_vlog" 2>&1 || exit 11; } &&
    { echo "== test build" >>"$_vlog"; cargo test --workspace --locked --no-run >>"$_vlog" 2>&1 || exit 12; } &&
    { echo "== test (crates that need no GPU, without the game lock)" >>"$_vlog"; cargo test --workspace --locked $GPU_EXCLUDE >>"$_vlog" 2>&1 || exit 12; } &&
    { echo "== clippy" >>"$_vlog"; cargo clippy --workspace --all-targets --locked -- -D warnings >>"$_vlog" 2>&1 || exit 13; } )
  VERIFY_CODE=$?
  if [ "$VERIFY_CODE" = 0 ]; then
    until mkdir "$RUNS/.lock" 2>/dev/null; do
      owner=$(cat "$RUNS/.lock/pid" 2>/dev/null)
      [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null && rm -rf "$RUNS/.lock"
      sleep 0.3
    done
    echo $$ > "$RUNS/.lock/pid"; echo integration-tests > "$RUNS/.lock/label"; echo "$LANDER_GPU_TOTAL" > "$RUNS/.lock/secs"
    ( export CARGO_BUILD_JOBS=12 && echo "== test (GPU crates, under the game lock)" >>"$_vlog" && gpu_crate_tests >>"$_vlog" 2>&1 ) || VERIFY_CODE=12
    rm -rf "$RUNS/.lock"
  fi
  if [ "$VERIFY_CODE" = 0 ]; then
    game_run_check >>"$_vlog" 2>&1 || VERIFY_CODE=14
    LAST_RUN_FOLDER=$(cat "$RUN_FOLDER_FILE" 2>/dev/null || true)
  fi

  cat "$_vlog" >> "$LOG"
  case $VERIFY_CODE in
    0)  VERIFY_FAILED_AT="" ;;
    11) VERIFY_FAILED_AT=repo-check ;;
    12) VERIFY_FAILED_AT=test ;;
    13) VERIFY_FAILED_AT=clippy ;;
    14) VERIFY_FAILED_AT=game-run ;;
    *)  VERIFY_FAILED_AT="step $VERIFY_CODE" ;;
  esac
  VERIFY_FAILING_CRATE=""
  VERIFY_INFRA=0
  if [ "$VERIFY_CODE" != 0 ] && lander_is_infra_failure "$_vlog"; then
    VERIFY_INFRA=1
  fi
  return "$VERIFY_CODE"
}

lander_rebase_list() {
  _base_sha=$1
  _candidates=$2
  REBASED_REFS=""
  REBASED_TIP=$_base_sha
  git checkout -q --detach "$_base_sha"
  for _r in $_candidates; do
    git checkout -q -B lander-tmp "$_r" 2>>"$LOG"
    if git rebase "$REBASED_TIP" >>"$LOG" 2>&1; then
      REBASED_TIP=$(git rev-parse HEAD)
      REBASED_REFS="$REBASED_REFS $_r"
    else
      _files=$(git diff --name-only --diff-filter=U | tr '\n' ' ')
      git rebase --abort >/dev/null 2>&1
      say "CONFLICT: $_r does not rebase onto main plus the batch (files: $_files), left out: its author must rebase it"
      echo "$_r" >> "$RUNS/landing.conflict"
    fi
  done
  REBASED_REFS=${REBASED_REFS# }
  git checkout -q --detach "$REBASED_TIP"
}

lander_requeue_unique() {
  _rq_ref=$1
  if ! grep -qxF "$_rq_ref" "$Q" 2>/dev/null; then
    echo "$_rq_ref" >> "$Q"
  fi
}

lander_count_words() {
  set -- $1
  echo "$#"
}

lander_split_halves() {
  _all=$1
  _n=$(lander_count_words "$_all")
  _half=$((_n / 2))
  SPLIT_LEFT=""
  SPLIT_RIGHT=""
  _i=0
  for _w in $_all; do
    _i=$((_i + 1))
    if [ "$_i" -le "$_half" ]; then
      SPLIT_LEFT="$SPLIT_LEFT $_w"
    else
      SPLIT_RIGHT="$SPLIT_RIGHT $_w"
    fi
  done
  SPLIT_LEFT=${SPLIT_LEFT# }
  SPLIT_RIGHT=${SPLIT_RIGHT# }
}

lander_find_crate_culprits() {
  _crate=$1
  _refs=$2
  CULPRIT_S=""
  CULPRIT_REST=""
  for _w in $_refs; do
    if git diff --name-only "$base...$_w" 2>/dev/null | grep -qE "(^|/)$_crate(/|$)"; then
      CULPRIT_S="$CULPRIT_S $_w"
    else
      CULPRIT_REST="$CULPRIT_REST $_w"
    fi
  done
  CULPRIT_S=${CULPRIT_S# }
  CULPRIT_REST=${CULPRIT_REST# }
}

lander_land_or_split() {
  _refs=$1
  _tries=${2:-0}
  [ -z "$_refs" ] && return 0

  lander_rebase_list "$base" "$_refs"
  _rebased=$REBASED_REFS
  [ -z "$_rebased" ] && return 0
  tip=$REBASED_TIP

  lander_verify "$_rebased"
  if [ "$VERIFY_CODE" = 0 ]; then
    if git push -q "$SOMMELIER_UPSTREAM" HEAD:main >>"$LOG" 2>&1; then
      sha=$(git rev-parse --short HEAD)
      base=$(git rev-parse HEAD)
      git push -q --force "$SOMMELIER_UPSTREAM" HEAD:refs/heads/integration/next >>"$LOG" 2>&1 || true
      say "LANDED as $sha on main: $_rebased"
      names=""
      for _r in $_rebased; do names="$names ${_r#*/}"; done
      BUS send LANDED "main is $sha:$names" --topic status --ref "$sha"
      _post=${LANDER_POST_LAND_HOOK:-${HERE:-}/post-land.sh}
      if [ -n "$_post" ] && [ -f "$_post" ]; then
        sh "$_post" "$sha" "${LAST_RUN_FOLDER:-}" >>"$LOG" 2>&1 || true
      fi
    else
      for _r in $_rebased; do lander_requeue_unique "$_r"; done
      say "PUSH FAILED for: $_rebased (main moved?), queued again"
    fi
    return 0
  fi

  if [ "${VERIFY_INFRA:-0}" = 1 ] && [ "$_tries" -lt "$LANDER_INFRA_RETRIES" ]; then
    say "INFRA RETRY ($((_tries + 1))/$LANDER_INFRA_RETRIES) at $VERIFY_FAILED_AT for: $_rebased"
    [ "$LANDER_RETRY_WAIT_SEC" -gt 0 ] 2>/dev/null && sleep "$LANDER_RETRY_WAIT_SEC"
    lander_land_or_split "$_rebased" "$((_tries + 1))"
    return $?
  fi

  _cnt=$(lander_count_words "$_rebased")
  if [ "$_cnt" -le 1 ]; then
    echo "$_rebased" >> "$RUNS/landing.failed"
    say "FAILED at $VERIFY_FAILED_AT for: $_rebased (log $LOG): $(grep -a -E "^(error|test .* FAILED|---- .* stdout)|panicked at|error\[" "$LOG" | tail -6 | tr '\n' '|' | cut -c1-500)"
    return 0
  fi

  if [ -n "${VERIFY_FAILING_CRATE:-}" ]; then
    lander_find_crate_culprits "$VERIFY_FAILING_CRATE" "$_rebased"
    if [ "$(lander_count_words "$CULPRIT_S")" = 1 ] && [ -n "$CULPRIT_REST" ]; then
      lander_land_or_split "$CULPRIT_REST" 0
      lander_land_or_split "$CULPRIT_S" 0
      return 0
    fi
  fi

  lander_split_halves "$_rebased"
  _left=$SPLIT_LEFT
  _right=$SPLIT_RIGHT
  lander_land_or_split "$_left" 0
  lander_land_or_split "$_right" 0
  return 0
}

batch() {
  lander_disk_guard || return 1
  LOG=$SW/logs/lander-$(date +%m%d-%H%M%S).log
  cd "$WT" || return 1
  git rebase --abort >/dev/null 2>&1; git cherry-pick --abort >/dev/null 2>&1
  if [ "${LANDER_SKIP_FETCH:-0}" != 1 ]; then
    git fetch -q "$SOMMELIER_UPSTREAM"
    git fetch -q "$SOMMELIER_FORK" 2>/dev/null || true
  fi
  git checkout -q --detach "$SOMMELIER_UPSTREAM/main"
  base=$(git rev-parse HEAD)
  tip=$base

  # Take the queue as a whole: appends that come after this go to a fresh queue file, none is lost.
  mv "$Q" "$Q.batch" && touch "$Q"
  : > "$Q.seen"
  batch_refs=""
  batch_count=0

  while IFS= read -r ref || [ -n "$ref" ]; do
    [ -z "$ref" ] && continue
    if ! git rev-parse -q --verify "$ref" >/dev/null 2>&1; then
      say "queue: $ref does not exist, dropped"
      continue
    fi
    full_sha=$(git rev-parse "$ref")
    if [ "$(git rev-list --count "$base..$full_sha")" = 0 ]; then
      say "queue: $ref is already on main, dropped"
      continue
    fi
    if grep -qxF "$ref $full_sha" "$Q.seen" 2>/dev/null; then
      continue
    fi
    echo "$ref $full_sha" >> "$Q.seen"

    # Guard against instructions smuggled into the tree: a branch that changes what steers agents,
    # builds or this harness is held until the coordinator has read its diff and approved that commit.
    sha=$(git rev-parse --short "$ref")
    risky=$(git diff --name-only "$base...$ref" | grep -E "$GUARD" | tr '\n' ' ')
    if [ -n "$risky" ] &&
       ! grep -qxF "$ref $sha" "$RUNS/landing.approved" 2>/dev/null &&
       ! grep -qxF "$ref $full_sha" "$RUNS/landing.approved" 2>/dev/null; then
      if ! grep -qxF "$ref $sha" "$RUNS/landing.held" 2>/dev/null; then
        say "HELD: $ref at $sha changes files that steer agents, builds or the harness ($risky). Read the diff, then: echo '$ref $sha' >> $RUNS/landing.approved and queue it again"
        echo "$ref $sha" >> "$RUNS/landing.held"
      fi
      lander_requeue_unique "$ref"
      continue
    fi

    if ! lander_licence_marker_scan "$base" "$ref"; then
      echo "$ref" >> "$RUNS/landing.failed"
      say "FAILED at clean-room (licence marker) for: $ref (log $LOG)"
      continue
    fi

    if [ "$batch_count" -ge "$LANDER_MAX_BATCH" ]; then
      lander_requeue_unique "$ref"
      continue
    fi

    batch_refs="$batch_refs $ref"
    batch_count=$((batch_count + 1))
  done < "$Q.batch"

  batch_refs=${batch_refs# }
  [ -z "$batch_refs" ] && return 0
  echo "batch: $batch_refs" >> "$LOG"
  lander_land_or_split "$batch_refs" 0
}
