#!/bin/sh
# Shared library for lander.sh: game run gate, disk guard, licence scan,
# queue preparation, verification wrapper, and algorithm 4.4 batch splitting.

# With a result, fd 3 holds title.log and fd 4 holds run.txt.
game_run_decision() {
  if [ "${LANDER_GAME_RUN:-1}" = 0 ]; then
    echo "skip (LANDER_GAME_RUN=0)"
  elif ! printf '%s\n' "$1" | grep -qEv '^(docs/|\.github/)|\.md$|^$'; then
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
# This list also holds the files that agents and tools read. It must stay equal to the hold list of the hub
# (bus/src/gate/rules.rs) and to GUARDED_PATHS in templates/workflows/factory-batch.yml.
GUARD='(^|/)(\.github|\.claude|\.cursor|\.devcontainer)/|(^|/)(CLAUDE|AGENTS)\.md$|(^|/)(AGENTS\.override\.md|CLAUDE\.local\.md|\.cursorrules|\.windsurfrules|\.clinerules|\.envrc)$|(^|/)\.?(rustfmt|clippy)\.toml$|(^|/)deny\.toml$|^(swarm|sommelier|bus)/|(^|/)build\.rs$|\.(sh|ps1|cmd|bat)$|(^|/)Cargo\.lock$|(^|/)decant-repo-check/'

# Only the lander and a fixed vocabulary reach $FIND, because that file wakes an agent. Text that a
# patch or a compiler can shape (paths, compiler output) goes to lander.log and the wake file gets a
# pointer. Control characters are removed from everything that is written, so a line stays one line.
lander_clean() {
  printf '%s' "$*" | tr -d '\000-\037\177' | cut -c1-300
}

# A ref name from the queue, reduced to the characters a ref needs.
lander_safe_ref() {
  printf '%s' "$1" | tr -cd 'A-Za-z0-9._/:@+ -'
}

say() {
  _say_t=$(lander_clean "$*")
  echo "LANDER $_say_t" >> "$FIND"
  echo "$(date '+%F %T') $_say_t" >> "$RUNS/lander.log"
}

# say_detail <fixed text> <detail>: the wake file gets the fixed text and a pointer to lander.log, the log gets both.
say_detail() {
  _sd_t=$(lander_clean "$1")
  shift
  echo "LANDER $_sd_t (details in $RUNS/lander.log)" >> "$FIND"
  echo "$(date '+%F %T') $_sd_t: $(lander_clean "$*")" >> "$RUNS/lander.log"
}

# lander_changed <range>: the changed paths of a range, one per line. The list comes NUL separated with
# path quoting off, so a path with a character outside ASCII is the real name and the guard patterns
# match it. A path that holds a newline falls into pieces, and a piece can only add a match, never hide one.
# Renames are listed as a delete and an add, so the old path of a moved guarded file shows too.
# Returns 1 when git cannot give the list.
lander_changed() {
  _lc_t="${TMPDIR:-/tmp}/lander-changed.$$"
  if git -c core.quotePath=false diff -z --name-only --no-renames "$1" > "$_lc_t" 2>/dev/null; then
    tr '\0' '\n' < "$_lc_t"
    _lc_r=0
  else
    _lc_r=1
  fi
  rm -f "$_lc_t"
  return "$_lc_r"
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
  changed=$(lander_changed "$base...$tip") || return 1
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

# --- Bundle entries and the second lock (docs/factory.md 8.5) ---
# A queue line is a ref as before, or a bundle entry:
#   bundle:<sha256> <head sha> [key=value ...] [title=<text>]
# with the keys member_handle, member_id, order, submission and email. The title takes the rest of the line.

lander_gitref() {
  case $1 in
    bundle:*) printf 'refs/submissions/%s\n' "${1#bundle:}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

lander_entry_line() {
  awk -v r="$1" '$1 == r { print; exit }' "$Q.entries" 2>/dev/null
}

# lander_entry_get <line> <key>: the value of key=value in the line, or nothing.
lander_entry_get() {
  _eg_line=$1
  if [ "$2" = title ]; then
    case $_eg_line in *" title="*) printf '%s\n' "${_eg_line#* title=}" ;; esac
    return 0
  fi
  _eg_head=${_eg_line%% title=*}
  for _eg_w in $_eg_head; do
    case $_eg_w in "$2="*) printf '%s\n' "${_eg_w#*=}"; return 0 ;; esac
  done
}

lander_safe_word() {
  printf '%s' "$1" | tr -cd 'A-Za-z0-9._+@-'
}

# lander_commit_message <entry line>: the message of a landed submission. Only the entry and the
# lander write it. The commit messages of the patch never get here.
lander_commit_message() {
  _cm_hash=${1%% *}
  _cm_hash=${_cm_hash#bundle:}
  _cm_title=$(lander_entry_get "$1" title | tr -d '\000-\037\177' | cut -c1-200 | sed 's/^ *//; s/ *$//')
  [ -n "$_cm_title" ] || _cm_title="Land submission $(printf '%s' "$_cm_hash" | cut -c1-12)"
  _cm_handle=$(lander_safe_word "$(lander_entry_get "$1" member_handle)")
  _cm_id=$(lander_safe_word "$(lander_entry_get "$1" member_id)")
  _cm_order=$(lander_safe_word "$(lander_entry_get "$1" order)")
  _cm_sub=$(lander_safe_word "$(lander_entry_get "$1" submission)")
  _cm_mail=$(lander_safe_word "$(lander_entry_get "$1" email)")
  [ -n "$_cm_handle" ] || _cm_handle=unknown
  [ -n "$_cm_id" ] || _cm_id=unknown
  [ -n "$_cm_order" ] || _cm_order=unknown
  [ -n "$_cm_sub" ] || _cm_sub=$_cm_hash
  case $_cm_mail in *?@?*) ;; *) _cm_mail="$_cm_id@${LANDER_MEMBER_DOMAIN:-members.sommelier.invalid}" ;; esac
  printf '%s\n\n' "$_cm_title"
  printf 'Co-authored-by: %s <%s>\n' "$_cm_handle" "$_cm_mail"
  printf 'Factory-Order: %s\n' "$_cm_order"
  printf 'Factory-Submission: %s\n' "$_cm_sub"
  printf 'Signed-off-by: %s\n' "$_cm_handle"
}

# The address of the hub for plain http. No address is ever written to a log.
lander_hub_http() {
  if [ -n "${SOMMELIER_HUB_HTTP:-}" ]; then printf '%s\n' "$SOMMELIER_HUB_HTTP"; return 0; fi
  case ${SOMMELIER_URL:-} in
    wss://*) printf 'https://%s\n' "${SOMMELIER_URL#wss://}" ;;
    ws://*) printf 'http://%s\n' "${SOMMELIER_URL#ws://}" ;;
    *) printf '%s\n' "${SOMMELIER_LOCAL:-http://127.0.0.1:7300}" ;;
  esac
}

lander_download() {
  curl -fs --proto '=http,https' --max-time 300 --max-filesize 536870912 -o "$2" "$1"
}

lander_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

lander_nohooks_dir() {
  mkdir -p "$SW/nohooks"
  printf '%s\n' "$SW/nohooks"
}

lander_bundle_fail() {
  echo "$BF_REF" >> "$RUNS/landing.failed"
  say "FAILED at bundle ($1) for: $(lander_safe_ref "$BF_REF") ($2) (log $LOG)"
  rm -f "$BF_FILE"
  return 1
}

# lander_bundle_fetch <entry line>: downloads, checks and fetches a bundle into refs/submissions/<sha256>.
# On success BUNDLE_GREF names that ref. On failure the entry is in landing.failed and 1 comes back.
lander_bundle_fetch() {
  BF_REF=${1%% *}
  BF_FILE="$SW/bundle.$$"
  _bf_hash=${BF_REF#bundle:}
  _bf_want=""
  [ "$1" = "$BF_REF" ] || { _bf_want=${1#* }; _bf_want=${_bf_want%% *}; }
  BUNDLE_GREF=""
  case $_bf_hash in
    *[!0-9a-f]*|"") lander_bundle_fail entry "the name is not a sha256"; return 1 ;;
  esac
  [ "${#_bf_hash}" = 64 ] || { lander_bundle_fail entry "the name is not a sha256"; return 1; }
  case $_bf_want in
    *[!0-9a-f]*|"") lander_bundle_fail entry "the entry has no head sha"; return 1 ;;
  esac
  rm -f "$BF_FILE"
  if ! lander_download "$(lander_hub_http)/api/bundle/$_bf_hash" "$BF_FILE" || [ ! -s "$BF_FILE" ]; then
    lander_bundle_fail fetch "the hub did not serve the file"; return 1
  fi
  if [ "$(lander_sha256 "$BF_FILE")" != "$_bf_hash" ]; then
    lander_bundle_fail content "the sha256 of the file does not match its name"; return 1
  fi
  _bf_heads=$(git bundle list-heads "$BF_FILE" 2>/dev/null)
  if [ -z "$_bf_heads" ] || [ "$(printf '%s\n' "$_bf_heads" | wc -l | tr -d ' ')" != 1 ]; then
    lander_bundle_fail content "the bundle must hold exactly one ref"; return 1
  fi
  _bf_bref=${_bf_heads#* }
  if ! git check-ref-format "$_bf_bref" 2>/dev/null; then
    lander_bundle_fail content "the ref name in the bundle is not valid"; return 1
  fi
  git update-ref -d "refs/submissions/$_bf_hash" 2>/dev/null
  if ! git -c core.hooksPath="$(lander_nohooks_dir)" -c transfer.fsckObjects=true fetch -q --no-tags "$BF_FILE" "+$_bf_bref:refs/submissions/$_bf_hash" >>"$LOG" 2>&1; then
    lander_bundle_fail content "the bundle does not fetch"; return 1
  fi
  rm -f "$BF_FILE"
  _bf_head=$(git rev-parse -q --verify "refs/submissions/$_bf_hash^{commit}" 2>/dev/null)
  if [ "$_bf_head" != "$_bf_want" ]; then
    lander_bundle_fail content "the head of the bundle is not the sha of the entry"; return 1
  fi
  BUNDLE_GREF="refs/submissions/$_bf_hash"
}

# lander_squash_entry <bundle ref> <onto sha>: one commit on top of onto with the content of the whole
# submission. On failure the tree is clean again and SQUASH_FILES names the files in conflict.
lander_squash_entry() {
  _sq_g=$(lander_gitref "$1")
  _sq_hp=$(lander_nohooks_dir)
  SQUASH_FILES=""
  git checkout -q --detach "$2" || return 1
  if ! git -c core.hooksPath="$_sq_hp" merge --squash "$_sq_g" >>"$LOG" 2>&1; then
    SQUASH_FILES=$(git -c core.quotePath=false diff --name-only --diff-filter=U | tr '\n' ' ')
    git reset -q --hard "$2"
    return 1
  fi
  if git diff --cached --quiet; then
    SQUASH_FILES="no change is left after the squash"
    git reset -q --hard "$2"
    return 1
  fi
  lander_commit_message "$(lander_entry_line "$1")" > "$SW/squash.msg"
  if ! git -c core.hooksPath="$_sq_hp" commit -q --no-verify -F "$SW/squash.msg" >>"$LOG" 2>&1; then
    SQUASH_FILES="the commit failed"
    git reset -q --hard "$2"
    return 1
  fi
}

# The conclusion GitHub gives the batch check on a commit, or nothing while there is none.
# A check counts only when it has the right name, was made by the right app (SOMMELIER_BATCH_APP, default
# github-actions) and belongs to a workflow run of the right workflow file (SOMMELIER_BATCH_WORKFLOW).
# The name alone proves nothing: a patch can add a job with the same name. The real protection is the
# ruleset that requires the workflow from the default branch (swarm/README.md, second lock).
lander_check_conclusion() {
  _cc_name=$(lander_safe_word "${SOMMELIER_BATCH_CHECK:-factory-batch}")
  _cc_app=$(lander_safe_word "${SOMMELIER_BATCH_APP:-github-actions}")
  _cc_wf=$(printf '%s' "${SOMMELIER_BATCH_WORKFLOW:-.github/workflows/factory-batch.yml}" | tr -cd 'A-Za-z0-9._/-')
  _cc_repo=${SOMMELIER_GH_REPO:-'{owner}/{repo}'}
  _cc_suites=$(gh api "repos/$_cc_repo/actions/runs?head_sha=$1&per_page=100" \
    --jq ".workflow_runs[] | select((.path // \"\") | contains(\"$_cc_wf\")) | .check_suite_id" 2>>"$LOG" | tr -cd '0-9\n' | grep -v '^$' | tr '\n' ',' || true)
  [ -n "$_cc_suites" ] || return 0
  gh api "repos/$_cc_repo/commits/$1/check-runs?check_name=$_cc_name&per_page=100" \
    --jq ".check_runs | map(select(.name == \"$_cc_name\" and .app.slug == \"$_cc_app\" and ([.check_suite.id] | inside([${_cc_suites%,}])))) | sort_by(.id) | last | .conclusion // empty" 2>>"$LOG" || true
}

# lander_second_lock <refs>: pushes HEAD as landing/<batch id> and waits for GitHub to pass it.
# Returns 0 when the check succeeded, 1 when it ended with another conclusion (SECOND_LOCK_CONCLUSION),
# 2 when nothing could be decided (nothing was pushed to main). The landing branch goes in every case.
lander_second_lock() {
  _sl_sha=$(git rev-parse HEAD)
  _sl_branch="landing/${_batch_id:-batch-$$}"
  _sl_name=${SOMMELIER_BATCH_CHECK:-factory-batch}
  SECOND_LOCK_CONCLUSION=""
  if ! git push -q -f "$SOMMELIER_UPSTREAM" "HEAD:refs/heads/$_sl_branch" >>"$LOG" 2>&1; then
    say "SECOND LOCK: could not push $_sl_branch, nothing was pushed to main, queued again: $(lander_safe_ref "$1")"
    return 2
  fi
  _sl_deadline=$(( $(date +%s) + ${SOMMELIER_BATCH_CHECK_SECS:-1800} ))
  while :; do
    SECOND_LOCK_CONCLUSION=$(lander_check_conclusion "$_sl_sha")
    [ -n "$SECOND_LOCK_CONCLUSION" ] && break
    [ "$(date +%s)" -ge "$_sl_deadline" ] && break
    sleep "${SOMMELIER_BATCH_CHECK_PAUSE:-15}"
  done
  git push -q "$SOMMELIER_UPSTREAM" --delete "refs/heads/$_sl_branch" >>"$LOG" 2>&1 ||
    say "SECOND LOCK: could not delete $_sl_branch, delete it by hand"
  case $SECOND_LOCK_CONCLUSION in
    success) return 0 ;;
    "")
      say "SECOND LOCK: no answer for the check $_sl_name on $(git rev-parse --short "$_sl_sha") within ${SOMMELIER_BATCH_CHECK_SECS:-1800} s, nothing was pushed to main, queued again: $(lander_safe_ref "$1")"
      return 2 ;;
    *) return 1 ;;
  esac
}

lander_rebase_list() {
  _base_sha=$1
  _candidates=$2
  REBASED_REFS=""
  REBASED_TIP=$_base_sha
  git checkout -q --detach "$_base_sha"
  for _r in $_candidates; do
    case $_r in
      bundle:*)
        if lander_squash_entry "$_r" "$REBASED_TIP"; then
          REBASED_TIP=$(git rev-parse HEAD)
          REBASED_REFS="$REBASED_REFS $_r"
        else
          say_detail "CONFLICT: $(lander_safe_ref "$_r") does not squash onto main plus the batch, left out: its author must rebase it" "files: $SQUASH_FILES"
          echo "$_r" >> "$RUNS/landing.conflict"
        fi
        continue ;;
    esac
    git checkout -q -B lander-tmp "$_r" 2>>"$LOG"
    if git rebase "$REBASED_TIP" >>"$LOG" 2>&1; then
      REBASED_TIP=$(git rev-parse HEAD)
      REBASED_REFS="$REBASED_REFS $_r"
    else
      _files=$(git -c core.quotePath=false diff --name-only --diff-filter=U | tr '\n' ' ')
      git rebase --abort >/dev/null 2>&1
      say_detail "CONFLICT: $(lander_safe_ref "$_r") does not rebase onto main plus the batch, left out: its author must rebase it" "files: $_files"
      echo "$_r" >> "$RUNS/landing.conflict"
    fi
  done
  REBASED_REFS=${REBASED_REFS# }
  git checkout -q --detach "$REBASED_TIP"
}

lander_requeue_unique() {
  _rq_ref=$1
  _rq_line=$_rq_ref
  case $_rq_ref in
    bundle:*) _rq_e=$(lander_entry_line "$_rq_ref"); [ -n "$_rq_e" ] && _rq_line=$_rq_e ;;
  esac
  if ! awk -v r="$_rq_ref" '$1 == r { f = 1 } END { exit !f }' "$Q" 2>/dev/null; then
    echo "$_rq_line" >> "$Q"
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
    if lander_changed "$base...$(lander_gitref "$_w")" | grep -qE "(^|/)$_crate(/|$)"; then
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
  if [ "$VERIFY_CODE" = 0 ] && [ "${SOMMELIER_SECOND_LOCK:-0}" = 1 ]; then
    lander_second_lock "$_rebased"
    case $? in
      0) ;;
      1) VERIFY_CODE=15; VERIFY_FAILED_AT="second lock (${SECOND_LOCK_CONCLUSION:-no conclusion})"; VERIFY_INFRA=0 ;;
      *) for _r in $_rebased; do lander_requeue_unique "$_r"; done; return 0 ;;
    esac
  fi
  if [ "$VERIFY_CODE" = 0 ]; then
    if git push -q "$SOMMELIER_UPSTREAM" HEAD:main >>"$LOG" 2>&1; then
      sha=$(git rev-parse --short HEAD)
      base=$(git rev-parse HEAD)
      git push -q --force "$SOMMELIER_UPSTREAM" HEAD:refs/heads/integration/next >>"$LOG" 2>&1 || true
      say "LANDED as $sha on main: $(lander_safe_ref "$_rebased")"
      names=""
      for _r in $_rebased; do names="$names ${_r#*/}"; done
      BUS send LANDED "main is $sha:$names" --topic status --ref "$sha"
      _post=${LANDER_POST_LAND_HOOK:-${HERE:-}/post-land.sh}
      if [ -n "$_post" ] && [ -f "$_post" ]; then
        sh "$_post" "$sha" "${LAST_RUN_FOLDER:-}" >>"$LOG" 2>&1 || true
      fi
    else
      for _r in $_rebased; do lander_requeue_unique "$_r"; done
      say "PUSH FAILED for: $(lander_safe_ref "$_rebased") (main moved?), queued again"
    fi
    return 0
  fi

  if [ "${VERIFY_INFRA:-0}" = 1 ] && [ "$_tries" -lt "$LANDER_INFRA_RETRIES" ]; then
    say "INFRA RETRY ($((_tries + 1))/$LANDER_INFRA_RETRIES) at $VERIFY_FAILED_AT for: $(lander_safe_ref "$_rebased")"
    [ "$LANDER_RETRY_WAIT_SEC" -gt 0 ] 2>/dev/null && sleep "$LANDER_RETRY_WAIT_SEC"
    lander_land_or_split "$_rebased" "$((_tries + 1))"
    return $?
  fi

  _cnt=$(lander_count_words "$_rebased")
  if [ "$_cnt" -le 1 ]; then
    echo "$_rebased" >> "$RUNS/landing.failed"
    say "FAILED at $VERIFY_FAILED_AT for: $(lander_safe_ref "$_rebased") (read the compiler and test output in the log $LOG)"
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
  : > "$Q.entries"
  batch_refs=""
  batch_count=0

  while IFS= read -r entry || [ -n "$entry" ]; do
    [ -z "$entry" ] && continue
    ref=${entry%% *}
    gref=$ref
    case $ref in
      bundle:*)
        printf '%s\n' "$entry" >> "$Q.entries"
        lander_bundle_fetch "$entry" || continue
        gref=$BUNDLE_GREF ;;
      *)
        if ! git rev-parse -q --verify "$ref" >/dev/null 2>&1; then
          say "queue: $(lander_safe_ref "$ref") does not exist, dropped"
          continue
        fi ;;
    esac
    full_sha=$(git rev-parse "$gref")
    if [ "$(git rev-list --count "$base..$full_sha")" = 0 ]; then
      say "queue: $(lander_safe_ref "$ref") is already on main, dropped"
      continue
    fi
    if grep -qxF "$ref $full_sha" "$Q.seen" 2>/dev/null; then
      continue
    fi
    echo "$ref $full_sha" >> "$Q.seen"

    # Guard against instructions smuggled into the tree: a branch that changes what steers agents,
    # builds or this harness is held until the coordinator has read its diff and approved that commit.
    sha=$(git rev-parse --short "$gref")
    # A diff that cannot be read counts as risky: the guard never fails open.
    if _changed=$(lander_changed "$base...$gref"); then
      risky=$(printf '%s\n' "$_changed" | grep -E "$GUARD" | tr '\n' ' ')
    else
      risky="the diff could not be read"
    fi
    if [ -n "$risky" ] &&
       ! grep -qxF "$ref $sha" "$RUNS/landing.approved" 2>/dev/null &&
       ! grep -qxF "$ref $full_sha" "$RUNS/landing.approved" 2>/dev/null; then
      if ! grep -qxF "$ref $sha" "$RUNS/landing.held" 2>/dev/null; then
        say_detail "HELD: $(lander_safe_ref "$ref") at $sha changes files that steer agents, builds or the harness. Read the diff (git diff $base...$sha), then: echo '$(lander_safe_ref "$ref") $sha' >> $RUNS/landing.approved and queue it again" "paths: $risky"
        echo "$ref $sha" >> "$RUNS/landing.held"
      fi
      lander_requeue_unique "$ref"
      continue
    fi

    if ! lander_licence_marker_scan "$base" "$gref"; then
      echo "$ref" >> "$RUNS/landing.failed"
      say "FAILED at clean-room (licence marker) for: $(lander_safe_ref "$ref") (log $LOG)"
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
