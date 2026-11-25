#!/bin/sh
# Reads one game run folder and appends one TSV row to $SOMMELIER_RUNS/metrics.tsv.
# Usage: metrics.sh <run_folder>
set -u

if [ $# -lt 1 ]; then
  echo "usage: $(basename "$0") <run_folder>" >&2
  exit 2
fi

RUN_DIR=${1%/}
if [ ! -d "$RUN_DIR" ]; then
  echo "metrics: not a directory: $RUN_DIR" >&2
  exit 2
fi

SOMMELIER_RUNS=${SOMMELIER_RUNS:-$(dirname "$(cd "$(dirname "$0")/.." && pwd)")/sommelier-runs}
TSV="$SOMMELIER_RUNS/metrics.tsv"
mkdir -p "$SOMMELIER_RUNS"

run_id=$(basename "$RUN_DIR")

if [ -f "$TSV" ] && awk -F'\t' -v id="$run_id" 'NR > 1 && $5 == id { found = 1; exit } END { exit !found }' "$TSV"; then
  exit 0
fi

RUN_TXT="$RUN_DIR/run.txt"
LOG="$RUN_DIR/title.log"
SUMMARY="$RUN_DIR/summary.txt"

clean_field() {
  printf '%s' "$1" | tr '\t\r\n' '   ' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

# utc: from run.txt "utc ..." or folder prefix YYYYMMDD-HHMMSS-...
utc="-"
if [ -r "$RUN_TXT" ]; then
  u=$(awk '/^utc[[:space:]]+/ { print $2; exit }' "$RUN_TXT")
  [ -n "$u" ] && utc="$u"
fi
if [ "$utc" = "-" ]; then
  u=$(printf '%s\n' "$run_id" | sed -n 's/^\([0-9]\{4\}\)\([0-9]\{2\}\)\([0-9]\{2\}\)-\([0-9]\{2\}\)\([0-9]\{2\}\)\([0-9]\{2\}\)-.*/\1-\2-\3T\4:\5:\6Z/p')
  [ -n "$u" ] && utc="$u"
fi

# sha: from run.txt "worktree <path> at <sha> ..."
sha="-"
if [ -r "$RUN_TXT" ]; then
  s=$(awk '/^worktree .* at [0-9a-fA-F]+/ { for (i = 1; i <= NF; i++) if ($i == "at" && i < NF) { print $(i + 1); exit } }' "$RUN_TXT")
  [ -n "$s" ] && sha="$s"
fi

# machine_class and scenario: from run.txt or env, else "-"
machine_class="-"
if [ -r "$RUN_TXT" ]; then
  mc=$(awk '/^machine_class[[:space:]]+/ { print $2; exit } /^SOMMELIER_MACHINE_CLASS=/ { sub(/^SOMMELIER_MACHINE_CLASS=/, ""); print; exit }' "$RUN_TXT")
  [ -n "$mc" ] && machine_class="$mc"
fi
[ "$machine_class" = "-" ] && [ -n "${SOMMELIER_MACHINE_CLASS:-}" ] && machine_class="$SOMMELIER_MACHINE_CLASS"

scenario="-"
if [ -r "$RUN_TXT" ]; then
  sc=$(awk '/^scenario[[:space:]]+/ { print $2; exit } /^SOMMELIER_SCENARIO=/ { sub(/^SOMMELIER_SCENARIO=/, ""); print; exit }' "$RUN_TXT")
  [ -n "$sc" ] && scenario="$sc"
fi
[ "$scenario" = "-" ] && [ -n "${SOMMELIER_SCENARIO:-}" ] && scenario="$SOMMELIER_SCENARIO"

# label: part after YYYYMMDD-HHMMSS- in run_id, else run_id
label=$(printf '%s\n' "$run_id" | sed -n 's/^[0-9]\{8\}-[0-9]\{6\}-\(.*\)/\1/p')
[ -z "$label" ] && label="$run_id"

# device_removed and unhandled counts
device_removed="-"
unhandled="-"
if [ -r "$LOG" ]; then
  device_removed=$(grep -a -c -i -E 'DEVICE_REMOVED|DEVICE_HUNG|device removed|device is gone|device was removed|Removing Device' "$LOG" 2>/dev/null || true)
  unhandled=$(grep -a -c 'UNHANDLED' "$LOG" 2>/dev/null || true)
  [ -z "$device_removed" ] && device_removed=0
  [ -z "$unhandled" ] && unhandled=0
elif [ -r "$SUMMARY" ]; then
  dl=$(sed -n 's/^device loss:[[:space:]]*\([0-9][0-9]*\) lines.*/\1/p' "$SUMMARY" | head -n 1)
  [ -n "$dl" ] && device_removed="$dl"
  if grep -q '^unhandled:' "$SUMMARY"; then
    uh=$(sed -n 's/^unhandled:[[:space:]]*//p' "$SUMMARY" | head -n 1)
    if [ -n "$uh" ]; then unhandled=1; else unhandled=0; fi
  fi
fi

# secs_alive and ended_by
secs_alive="-"
ended_by="-"
if [ -r "$RUN_TXT" ]; then
  self_secs=$(sed -n 's/^ended by itself after \([0-9][0-9]*\) s.*/\1/p' "$RUN_TXT" | head -n 1)
  cfg_secs=$(awk '/^seconds[[:space:]]+[0-9]+/ { print $2; exit }' "$RUN_TXT")
  if [ -n "$self_secs" ]; then
    secs_alive="$self_secs"
    if [ "$unhandled" != "-" ] && [ "$unhandled" -gt 0 ] 2>/dev/null; then
      ended_by="crash"
    elif [ "$device_removed" != "-" ] && [ "$device_removed" -gt 0 ] 2>/dev/null; then
      ended_by="device_removed"
    else
      ended_by="self"
    fi
  elif [ -n "$cfg_secs" ]; then
    secs_alive="$cfg_secs"
    ended_by="timeout"
  fi
fi
if [ "$secs_alive" = "-" ] && [ -r "$LOG" ]; then
  last_ts=$(awk '/^[[:space:]]*[0-9]+(\.[0-9]+)?[[:space:]]/ { ts = $1 } END { if (ts != "") printf "%.0f", ts }' "$LOG")
  [ -n "$last_ts" ] && secs_alive="$last_ts"
fi

# first_present_s, frames, fps_median, fps_p10 from title.log present probe lines
first_present_s="-"
frames="-"
fps_median="-"
fps_p10="-"
if [ -r "$LOG" ]; then
  probe_stats=$(awk '
    /present probe: frame [0-9]+/ {
      t = $1 + 0
      t_str = $1
      if (match($0, /present probe: frame [0-9]+/)) {
        s = substr($0, RSTART, RLENGTH)
        sub(/.*frame /, "", s)
        f = s + 0
        if (!have) {
          first_t = t_str
        } else if (t > pt && f > pf) {
          n++
          fps[n] = (f - pf) / (t - pt)
        }
        pt = t; pf = f; have = 1
      }
    }
    END {
      if (!have) {
        print "-\t-\t-\t-"
        exit
      }
      if (n > 0) {
        for (i = 2; i <= n; i++) {
          v = fps[i]; j = i - 1
          while (j >= 1 && fps[j] > v) {
            fps[j + 1] = fps[j]; j--
          }
          fps[j + 1] = v
        }
        med = (n % 2 == 1) ? fps[int((n + 1) / 2)] : (fps[n / 2] + fps[n / 2 + 1]) / 2
        idx10 = int(n * 0.1 + 0.5); if (idx10 < 1) idx10 = 1
        p10 = fps[idx10]
        printf "%s\t%d\t%.2f\t%.2f\n", first_t, pf, med, p10
      } else {
        printf "%s\t%d\t-\t-\n", first_t, pf
      }
    }
  ' "$LOG")
  IFS='	' read -r first_present_s frames fps_median fps_p10 <<EOF
$probe_stats
EOF
fi
if [ "$frames" = "-" ] && [ -r "$SUMMARY" ]; then
  sf=$(sed -n 's/^frames:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$SUMMARY" | head -n 1)
  [ -n "$sf" ] && frames="$sf"
fi

# peak_commit_gb: only if logged
peak_commit_gb="-"
for f in "$LOG" "$RUN_TXT" "$SUMMARY"; do
  [ -r "$f" ] || continue
  pc=$(sed -n 's/.*peak_commit_gb:[[:space:]]*\([0-9][0-9.]*\).*/\1/p' "$f" | head -n 1)
  [ -z "$pc" ] && pc=$(sed -n 's/.*peak commit:[[:space:]]*\([0-9][0-9.]*\)[[:space:]]*G.*/\1/p' "$f" | head -n 1)
  if [ -n "$pc" ]; then
    peak_commit_gb="$pc"
    break
  fi
done

# gentle and load_note from run.txt
gentle="-"
load_note="-"
if [ -r "$RUN_TXT" ]; then
  g=$(sed -n 's/^SOMMELIER_GENTLE=\([01]\).*/\1/p' "$RUN_TXT" | head -n 1)
  [ -z "$g" ] && g=$(sed -n 's/^gentle: SOMMELIER_GENTLE=\([01]\).*/\1/p' "$RUN_TXT" | head -n 1)
  [ -n "$g" ] && gentle="$g"
  ln=$(sed -n 's/^gentle: SOMMELIER_GENTLE=[01] (\(.*\))/\1/p' "$RUN_TXT" | head -n 1)
  [ -n "$ln" ] && load_note="$ln"
fi

utc=$(clean_field "$utc")
sha=$(clean_field "$sha")
machine_class=$(clean_field "$machine_class")
scenario=$(clean_field "$scenario")
run_id=$(clean_field "$run_id")
label=$(clean_field "$label")
secs_alive=$(clean_field "$secs_alive")
ended_by=$(clean_field "$ended_by")
device_removed=$(clean_field "$device_removed")
unhandled=$(clean_field "$unhandled")
first_present_s=$(clean_field "$first_present_s")
frames=$(clean_field "$frames")
fps_median=$(clean_field "$fps_median")
fps_p10=$(clean_field "$fps_p10")
peak_commit_gb=$(clean_field "$peak_commit_gb")
gentle=$(clean_field "$gentle")
load_note=$(clean_field "$load_note")

if [ ! -s "$TSV" ]; then
  printf 'utc\tsha\tmachine_class\tscenario\trun_id\tlabel\tsecs_alive\tended_by\tdevice_removed\tunhandled\tfirst_present_s\tframes\tfps_median\tfps_p10\tpeak_commit_gb\tgentle\tload_note\n' > "$TSV"
fi

printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
  "$utc" "$sha" "$machine_class" "$scenario" "$run_id" "$label" \
  "$secs_alive" "$ended_by" "$device_removed" "$unhandled" \
  "$first_present_s" "$frames" "$fps_median" "$fps_p10" \
  "$peak_commit_gb" "$gentle" "$load_note" >> "$TSV"
