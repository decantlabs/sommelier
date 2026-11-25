#!/bin/sh
# Self-contained tests for lander.sh and lander-lib.sh (no network, no cargo, no game run).

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lander-lib.sh"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/lander-test.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0

assert_eq() {
  _got=$1
  _want=$2
  _msg=$3
  if [ "$_got" = "$_want" ]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s (got=%s want=%s)\n' "$_msg" "$_got" "$_want"
  fi
}

assert_le() {
  _got=$1
  _max=$2
  _msg=$3
  if [ "$_got" -le "$_max" ]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s (got=%s max=%s)\n' "$_msg" "$_got" "$_max"
  fi
}

assert_file_has() {
  _file=$1
  _pat=$2
  _msg=$3
  if grep -qE "$_pat" "$_file" 2>/dev/null; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s (missing pattern %s in %s)\n' "$_msg" "$_pat" "$_file"
  fi
}

assert_file_not_has() {
  _file=$1
  _pat=$2
  _msg=$3
  if ! grep -qE "$_pat" "$_file" 2>/dev/null; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s (unexpected pattern %s in %s)\n' "$_msg" "$_pat" "$_file"
  fi
}

setup_env() {
  _name=$1
  _root="$TMP/$_name"
  mkdir -p "$_root/upstream.git" "$_root/origin.git" "$_root/wt" "$_root/runs" "$_root/state/logs"
  git init --bare -q "$_root/upstream.git"
  git -C "$_root/upstream.git" symbolic-ref HEAD refs/heads/main
  git init --bare -q "$_root/origin.git"

  git clone -q "$_root/upstream.git" "$_root/wt" 2>/dev/null
  git -C "$_root/wt" checkout -q -b main 2>/dev/null || true
  git -C "$_root/wt" config user.name "test"
  git -C "$_root/wt" config user.email "test@example.com"
  git -C "$_root/wt" remote add origin "$_root/origin.git"
  git -C "$_root/wt" remote rename origin upstream 2>/dev/null || true
  # Ensure remotes are named upstream and origin cleanly
  git -C "$_root/wt" remote remove upstream 2>/dev/null || true
  git -C "$_root/wt" remote remove origin 2>/dev/null || true
  git -C "$_root/wt" remote add upstream "$_root/upstream.git"
  git -C "$_root/wt" remote add origin "$_root/origin.git"

  printf 'init\n' > "$_root/wt/README.txt"
  git -C "$_root/wt" add README.txt
  git -C "$_root/wt" commit -q -m "initial"
  git -C "$_root/wt" push -q upstream main

  WT="$_root/wt"
  SW="$_root/state"
  RUNS="$_root/runs"
  Q="$RUNS/landing.queue"
  FIND="$RUNS/watchdog.findings"
  SOMMELIER_UPSTREAM="upstream"
  SOMMELIER_FORK="origin"
  SOMMELIER_URL=""
  LANDER_GAME_RUN=0
  LANDER_RETRY_WAIT_SEC=0
  LANDER_INFRA_RETRIES=1
  LANDER_DISK_FREE_GB=100
  LANDER_MIN_FREE_GB=40
  LANDER_SKIP_FETCH=0
  LANDER_POST_LAND_HOOK=""
  LANDER_CLEANUP_HOOK=""
  : > "$Q"
  : > "$FIND"
  : > "$RUNS/landing.failed"
  : > "$RUNS/landing.held"
  : > "$RUNS/landing.approved"
  : > "$RUNS/landing.conflict"
  VERIFY_COUNT_FILE="$_root/verify_count"
  echo 0 > "$VERIFY_COUNT_FILE"
}

make_branch() {
  _bname=$1
  _file=$2
  _content=$3
  git -C "$WT" checkout -q --detach upstream/main
  git -C "$WT" checkout -q -B "$_bname"
  mkdir -p "$(dirname "$WT/$_file")"
  printf '%s\n' "$_content" > "$WT/$_file"
  git -C "$WT" add "$_file"
  git -C "$WT" commit -q -m "commit $_bname"
  git -C "$WT" push -q -f origin "$_bname"
}

# Fake verify: fails if BAD_FILE exists in the checked-out tree.
# If INFRA_FILE exists and INFRA_FAIL_ONCE=1, fails once with "timed out" then passes.
lander_verify() {
  _refs=$1
  _vlog="$LOG.step"
  : > "$_vlog"
  _c=$(cat "$VERIFY_COUNT_FILE")
  _c=$((_c + 1))
  echo "$_c" > "$VERIFY_COUNT_FILE"

  VERIFY_CODE=0
  VERIFY_FAILED_AT=""
  VERIFY_FAILING_CRATE=""
  VERIFY_INFRA=0

  if [ -f "$WT/BAD.txt" ]; then
    echo "error[E0001]: synthetic bad branch" >> "$_vlog"
    VERIFY_CODE=12
    VERIFY_FAILED_AT=test
  elif [ -f "$WT/INFRA.txt" ] && [ "${INFRA_FAILS_LEFT:-0}" -gt 0 ]; then
    INFRA_FAILS_LEFT=$((INFRA_FAILS_LEFT - 1))
    echo "error: command timed out after 60s" >> "$_vlog"
    VERIFY_CODE=12
    VERIFY_FAILED_AT=test
    VERIFY_INFRA=1
  fi
  cat "$_vlog" >> "$LOG"
  return "$VERIFY_CODE"
}

# --- Test 1: Batch of 4 with 1 bad lands 3 in at most 5 verifications ---
setup_env t1
make_branch b1 "f1.txt" "ok1"
make_branch b2 "f2.txt" "ok2"
make_branch b3 "BAD.txt" "bad"
make_branch b4 "f4.txt" "ok4"
printf 'origin/b1\norigin/b2\norigin/b3\norigin/b4\n' >> "$Q"
batch
_vcount=$(cat "$VERIFY_COUNT_FILE")
assert_le "$_vcount" 5 "t1: batch of 4 with 1 bad uses at most 5 verifications"
assert_file_has "$RUNS/landing.failed" "^origin/b3$" "t1: bad branch b3 in landing.failed"
assert_file_not_has "$RUNS/landing.failed" "^origin/b(1|2|4)$" "t1: good branches not in landing.failed"
git -C "$WT" fetch -q upstream
_main_files=$(git -C "$WT" ls-tree --name-only upstream/main)
assert_eq "$(printf '%s\n' "$_main_files" | grep -cE '^f(1|2|4)\.txt$')" "3" "t1: 3 good branches landed on main"
assert_eq "$(printf '%s\n' "$_main_files" | grep -c '^BAD\.txt$')" "0" "t1: bad branch did not land on main"

# --- Test 2: Same ref and sha queued twice lands once ---
setup_env t2
make_branch dup "dup.txt" "one"
printf 'origin/dup\norigin/dup\n' >> "$Q"
batch
assert_eq "$(cat "$VERIFY_COUNT_FILE")" "1" "t2: duplicate ref+sha verified once"
assert_eq "$(git -C "$WT" rev-list --count upstream/main)" "2" "t2: main has initial + 1 landed commit"

# --- Test 3: Held ref checked every round, new sha asks again, approved lands ---
setup_env t3
make_branch held1 "swarm/sample.sh" "#!/bin/sh"
sha1=$(git -C "$WT" rev-parse --short origin/held1)
printf 'origin/held1\n' >> "$Q"
batch
assert_file_has "$FIND" "HELD: origin/held1 at $sha1" "t3: held ref announced at sha1"
assert_file_has "$Q" "^origin/held1$" "t3: held ref kept in queue for next round"
assert_eq "$(grep -c "HELD: origin/held1" "$FIND")" "1" "t3: held count is 1 after round 1"

# Round 2 with same sha: still held, not spammed
batch
assert_eq "$(grep -c "HELD: origin/held1" "$FIND")" "1" "t3: held not spammed for same sha"
assert_file_has "$Q" "^origin/held1$" "t3: held ref still in queue after round 2"

# Push new commit to held1 -> new sha asks again
make_branch held1 "swarm/sample.sh" "#!/bin/sh\necho v2"
sha2=$(git -C "$WT" rev-parse --short origin/held1)
batch
assert_file_has "$FIND" "HELD: origin/held1 at $sha2" "t3: held ref with new sha asks again"

# Approve sha2 -> lands on next round
printf 'origin/held1 %s\n' "$sha2" >> "$RUNS/landing.approved"
batch
assert_file_has "$FIND" "LANDED as .* on main: origin/held1" "t3: approved held ref lands"

# --- Test 4: Infra failure is retried once ---
setup_env t4
make_branch inf1 "INFRA.txt" "transient"
INFRA_FAILS_LEFT=1
printf 'origin/inf1\n' >> "$Q"
batch
assert_eq "$(cat "$VERIFY_COUNT_FILE")" "2" "t4: transient infra retried once (2 verifications)"
assert_file_has "$FIND" "LANDED as .* on main: origin/inf1" "t4: transient infra lands after retry"
assert_file_not_has "$RUNS/landing.failed" "^origin/inf1$" "t4: transient infra not in landing.failed"

# Persistent infra failure fails after 1 retry
setup_env t4b
make_branch inf2 "INFRA.txt" "persistent"
INFRA_FAILS_LEFT=5
printf 'origin/inf2\n' >> "$Q"
batch
assert_eq "$(cat "$VERIFY_COUNT_FILE")" "2" "t4b: persistent infra retried once then stops"
assert_file_has "$RUNS/landing.failed" "^origin/inf2$" "t4b: persistent infra recorded in landing.failed"

# --- Test 5: Low disk blocks with DISK_LOW ---
setup_env t5
make_branch disk1 "d1.txt" "ok"
printf 'origin/disk1\n' >> "$Q"
LANDER_DISK_FREE_GB=10
batch
assert_file_has "$FIND" "DISK_LOW" "t5: low disk emits DISK_LOW"
assert_file_has "$Q" "^origin/disk1$" "t5: low disk leaves queue intact"
assert_eq "$(cat "$VERIFY_COUNT_FILE")" "0" "t5: low disk skips verification"

# --- Test 6: Licence marker scan on added lines ---
setup_env t6
make_branch lic1 "src/bad.rs" "// SPDX-License-Identifier: GPL-2.0-only"
printf 'origin/lic1\n' >> "$Q"
batch
assert_file_has "$RUNS/landing.failed" "^origin/lic1$" "t6: licence marker puts branch in landing.failed"
assert_file_has "$FIND" "clean-room" "t6: licence marker reports clean-room"
assert_eq "$(cat "$VERIFY_COUNT_FILE")" "0" "t6: licence marker fails before verify"

# --- Test 7: post-land.sh hook is called when present ---
setup_env t7
_hook="$TMP/t7/post-land.sh"
_marker="$TMP/t7/post-land.called"
cat > "$_hook" <<EOF
#!/bin/sh
printf '%s\n' "\$1" > "$_marker"
EOF
LANDER_POST_LAND_HOOK="$_hook"
make_branch pl1 "pl.txt" "ok"
printf 'origin/pl1\n' >> "$Q"
batch
assert_file_has "$_marker" "^[0-9a-f]+" "t7: post-land.sh called with landed sha"

# --- Test 8: LANDER_DRY=1 output unchanged for old cases ---
_dry_out=$(LANDER_DRY=1 sh "$HERE/lander.sh")
assert_eq "$?" "0" "t8: LANDER_DRY=1 exits 0"
_want_dry="Docs and workflows: skip (only docs, workflows or Markdown changed)
Clean run: pass
Removing Device: refuse (Removing Device or UNHANDLED in title.log)
Title ended at 29 seconds: refuse (title ended before 55 seconds)
Disabled: skip (LANDER_GAME_RUN=0)
UNHANDLED: refuse (Removing Device or UNHANDLED in title.log)
Title ended at 55 seconds: pass
Missing run folder: refuse (missing run folder or logs)
GPU total deadline after 1500 seconds: gpu-fast-a tested; gpu-slow-b tested; fail (overall limit; not yet tested: gpu-fast-c)"
assert_eq "$_dry_out" "$_want_dry" "t8: LANDER_DRY=1 output matches baseline"

printf 'lander-test: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
