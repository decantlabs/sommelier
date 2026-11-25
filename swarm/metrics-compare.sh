#!/bin/sh
# Compares fps_median between two commits in $SOMMELIER_RUNS/metrics.tsv.
# Prints REGRESSION if >= MIN_RUNS (default 3) rows exist per commit and fps_median dropped by >= DROP_PCT (default 20%).
# Usage: metrics-compare.sh [<sha_new> [<sha_base>]]
set -u

SOMMELIER_RUNS=${SOMMELIER_RUNS:-$(dirname "$(cd "$(dirname "$0")/.." && pwd)")/sommelier-runs}
TSV=${METRICS_TSV:-$SOMMELIER_RUNS/metrics.tsv}
MIN_RUNS=${MIN_RUNS:-3}
DROP_PCT=${DROP_PCT:-20}

[ -r "$TSV" ] || exit 0

sha_new=${1:-}
sha_base=${2:-}

if [ -z "$sha_new" ] || [ -z "$sha_base" ]; then
  shas=$(awk -F'\t' -v target="$sha_new" '
    NR > 1 && $2 != "-" && $2 != "" {
      if (!seen[$2]++) order[++n] = $2
    }
    END {
      if (target != "") {
        for (i = n; i >= 1; i--) {
          if (order[i] == target && i > 1) {
            print target "\t" order[i - 1]
            exit
          }
        }
      } else if (n >= 2) {
        print order[n] "\t" order[n - 1]
      }
    }
  ' "$TSV")
  [ -n "$shas" ] || exit 0
  IFS='	' read -r sha_new sha_base <<EOF
$shas
EOF
fi

[ -n "$sha_new" ] && [ -n "$sha_base" ] || exit 0
[ "$sha_new" = "$sha_base" ] && exit 0

awk -F'\t' -v s_new="$sha_new" -v s_base="$sha_base" -v min_r="$MIN_RUNS" -v drop_p="$DROP_PCT" '
  function median(arr, cnt,    i, j, v) {
    for (i = 2; i <= cnt; i++) {
      v = arr[i]; j = i - 1
      while (j >= 1 && arr[j] > v) {
        arr[j + 1] = arr[j]; j--
      }
      arr[j + 1] = v
    }
    if (cnt % 2 == 1) return arr[int((cnt + 1) / 2)]
    return (arr[cnt / 2] + arr[cnt / 2 + 1]) / 2
  }
  NR > 1 && $13 ~ /^[0-9]+(\.[0-9]+)?$/ {
    if ($2 == s_new) {
      n_new++; v_new[n_new] = $13 + 0
    } else if ($2 == s_base) {
      n_base++; v_base[n_base] = $13 + 0
    }
  }
  END {
    if (n_new < min_r || n_base < min_r) exit 0
    m_base = median(v_base, n_base)
    m_new = median(v_new, n_new)
    if (m_base <= 0) exit 0
    drop = (m_base - m_new) * 100.0 / m_base
    if (drop >= drop_p) {
      printf "REGRESSION sha=%s base=%s metric=fps_median base=%.2f new=%.2f drop=%.1f%%\n", s_new, s_base, m_base, m_new, drop
    }
  }
' "$TSV"
