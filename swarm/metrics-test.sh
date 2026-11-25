#!/bin/sh
# Tests metrics.sh, metrics-compare.sh, and post-land.sh on synthetic run folders.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

export SOMMELIER_RUNS="$TMP"
TSV="$TMP/metrics.tsv"

# Synthetic run 1: normal timeout run at 30.00 fps
R1="$TMP/20261125-100000-synth-a"
mkdir -p "$R1"
cat > "$R1/run.txt" <<'EOF'
worktree /fake/wt at aaa1111 synth commit a
seconds 60
SOMMELIER_GENTLE=0
gentle: SOMMELIER_GENTLE=0 (forced by GENTLE=0)
EOF
cat > "$R1/title.log" <<'EOF'
   2.000 debug d3d12_xs: present probe: frame 1 (thread 1), resource 0x1: 0 of 100 pixels
   3.000 debug d3d12_xs: present probe: frame 31 (thread 1), resource 0x1: 100 of 100 pixels
   4.000 debug d3d12_xs: present probe: frame 61 (thread 1), resource 0x1: 100 of 100 pixels
   5.000 debug d3d12_xs: present probe: frame 91 (thread 1), resource 0x1: 100 of 100 pixels
EOF

# Synthetic run 2: crash with device loss and unhandled exception at 20.00 fps
R2="$TMP/20261125-100500-synth-b"
mkdir -p "$R2"
cat > "$R2/run.txt" <<'EOF'
worktree /fake/wt at bbb2222 synth commit b
seconds 120
SOMMELIER_GENTLE=1
gentle: SOMMELIER_GENTLE=1 (auto, on because running: cargo.exe )
ended by itself after 45 s
EOF
cat > "$R2/title.log" <<'EOF'
   1.500 debug d3d12_xs: present probe: frame 1 (thread 1), resource 0x1: 0 of 100 pixels
   2.500 debug d3d12_xs: present probe: frame 21 (thread 1), resource 0x1: 100 of 100 pixels
   3.500 debug d3d12_xs: present probe: frame 41 (thread 1), resource 0x1: 100 of 100 pixels
  44.000 error d3d12_xs: Removing Device
  44.500 error exception 0xc0000005 UNHANDLED
EOF

# Synthetic run 3: minimal folder with no title.log (missing fields become "-")
R3="$TMP/20261125-101000-synth-c"
mkdir -p "$R3"
cat > "$R3/run.txt" <<'EOF'
worktree /fake/wt at ccc3333 synth commit c
seconds 30
EOF

sh "$HERE/metrics.sh" "$R1"
sh "$HERE/metrics.sh" "$R2"
sh "$HERE/metrics.sh" "$R3"

# Verify exact TSV contents
EXPECTED="$TMP/expected.tsv"
cat > "$EXPECTED" <<'EOF'
utc	sha	machine_class	scenario	run_id	label	secs_alive	ended_by	device_removed	unhandled	first_present_s	frames	fps_median	fps_p10	peak_commit_gb	gentle	load_note
2026-11-25T10:00:00Z	aaa1111	-	-	20261125-100000-synth-a	synth-a	60	timeout	0	0	2.000	91	30.00	30.00	-	0	forced by GENTLE=0
2026-11-25T10:05:00Z	bbb2222	-	-	20261125-100500-synth-b	synth-b	45	crash	1	1	1.500	41	20.00	20.00	-	1	auto, on because running: cargo.exe
2026-11-25T10:10:00Z	ccc3333	-	-	20261125-101000-synth-c	synth-c	30	timeout	-	-	-	-	-	-	-	-	-
EOF

if ! diff -u "$EXPECTED" "$TSV"; then
  echo "FAIL: metrics.tsv does not match expected output" >&2
  exit 1
fi

# Idempotent append: running metrics.sh again on R1 must not add a duplicate row
sh "$HERE/metrics.sh" "$R1"
if ! diff -u "$EXPECTED" "$TSV"; then
  echo "FAIL: duplicate row appended on second metrics.sh call" >&2
  exit 1
fi

# Compare with 1 row per commit: must print nothing even for a >20% drop
out_1row=$(sh "$HERE/metrics-compare.sh" bbb2222 aaa1111)
if [ -n "$out_1row" ]; then
  echo "FAIL: expected empty output for 1 row per commit, got: $out_1row" >&2
  exit 1
fi

# Add 2 more rows for aaa1111 (30.00 fps) and 3 rows for ddd4444 (20.00 fps, 33.3% drop)
# and 3 rows for eee5555 (28.00 fps, 6.7% drop)
for i in 2 3; do
  printf '2026-11-25T10:2%d:00Z\taaa1111\t-\t-\t20261125-102%d00-a%d\ta%d\t60\ttimeout\t0\t0\t2.000\t91\t30.00\t30.00\t-\t0\t-\n' "$i" "$i" "$i" "$i" >> "$TSV"
done
for i in 1 2 3; do
  printf '2026-11-25T10:3%d:00Z\teee5555\t-\t-\t20261125-103%d00-e%d\te%d\t60\ttimeout\t0\t0\t2.000\t91\t28.00\t28.00\t-\t0\t-\n' "$i" "$i" "$i" "$i" >> "$TSV"
done
for i in 1 2 3; do
  printf '2026-11-25T10:4%d:00Z\tddd4444\t-\t-\t20261125-104%d00-d%d\td%d\t60\ttimeout\t0\t0\t2.000\t91\t20.00\t20.00\t-\t0\t-\n' "$i" "$i" "$i" "$i" >> "$TSV"
done

# 3 rows with <20% drop: must print nothing
out_ok=$(sh "$HERE/metrics-compare.sh" eee5555 aaa1111)
if [ -n "$out_ok" ]; then
  echo "FAIL: expected no REGRESSION for <20% drop, got: $out_ok" >&2
  exit 1
fi

# 3 rows with >=20% drop: must print REGRESSION
out_reg=$(sh "$HERE/metrics-compare.sh" ddd4444 aaa1111)
case "$out_reg" in
  REGRESSION*) ;;
  *)
    echo "FAIL: expected REGRESSION for 3-row 33% drop, got: '$out_reg'" >&2
    exit 1
    ;;
esac

# post-land.sh on a synthetic run folder should succeed
sh "$HERE/post-land.sh" "$R1" >/dev/null

echo "metrics-test: PASS"
