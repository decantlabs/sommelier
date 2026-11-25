#!/usr/bin/env bash
# Runs a table of Antigravity (agy) workers until all of them are finished.
#
#   gemini-swarm.sh            reads $SOMMELIER_RUNS/gemini/tasks.tsv and starts the workers
#   DRY=1 gemini-swarm.sh      prints decisions for a synthetic task table without launching workers
#
# tasks.tsv, tab separated, one task per line (# starts a comment):
#   name  class  model id  effort  scope  depends on (name or -)  prompt file  [exclusive]
# The last column is optional (old tables without it still work). The word "exclusive" in it marks
# a run task that needs a clean machine. Any other value or none means not exclusive.
# Classes:
#   read   reads files and writes a report
#   build  compiles Rust: at most MAX_BUILD (6) at the same time, none while an exclusive run task is
#          going or is ready to start (a ready exclusive run task first waits for the builds to end)
#   run    needs a game run (through run-title.sh): one at a time. An exclusive one starts only when no
#          build worker runs. A run task that is not exclusive does not block builds.
# A task with a dependency waits until the worker it depends on has ended (state done or failed).
# Worker markers are the ones agy-agent.sh makes in $SOMMELIER_RUNS/agy: <name>.pid while it runs and
# <name>.done when it ended. Another tool may move <name>.done to <name>.done.reported before the
# launcher looks, so the launcher also reads that file and the answer file <name>.last.txt.
# The launcher keeps its own state per task in $SOMMELIER_RUNS/gemini/state.
# List scope paths as whitespace separated words. Active scopes reserve those paths.
# Worker time counts towards the local calendar day, including time across midnight.
# Quota per model family: when a worker's error file shows a quota or rate limit message, only the
# FAMILY of the model it used (the id without the effort suffix, e.g. claude-opus-5-5) is limited for
# 5 h 1 min (LIMIT_WAIT seconds). limited.tsv holds family, resume epoch and resume time and survives a
# restart, limit.txt is a short human summary. Every quota message also pauses all starts for 30 minutes.
# After the pause, only low and medium tasks may fall back. A task whose
# family is limited starts on the next model of a fixed chain (FALLBACK line in the log):
#   claude-opus-5-5 > gemini-4-neon > gemini-4-carbon > gemini-4-hydrogen > gemini-4-argon > gemini-3.8-flash
#   claude-sonnet-5-5 > gemini-4-hydrogen. The last of a chain, and models without a chain, wait.
# The effort word stays when the new model has it, else the highest it has. The model really used is
# the last column of results.tsv. The log is launcher.log, the pid launcher.pid.
# Use Bash even when invoked through sh.
[ -n "${BASH_VERSION:-}" ] || exec bash "$0" "$@"
set -u

# Maximum workers running across all classes and model families.
MAX_PARALLEL=${MAX_PARALLEL:-4}
# Maximum workers running on the same model family.
MAX_PER_FAMILY=${MAX_PER_FAMILY:-2}
# Worker minutes available per local calendar day.
DAILY_BUDGET_MIN=${DAILY_BUDGET_MIN:-600}
# Stop new starts at this percentage of the daily worker budget.
BUDGET_STOP_PCT=${BUDGET_STOP_PCT:-80}

HERE=$(cd "$(dirname "$0")" && pwd)
DRY=${DRY:-0}
if [ "$DRY" = 1 ]; then
  # The dependency column selects an independent scheduler snapshot.
  exec 3<<'TASKS'
available	read	gemini-4-carbon-low	low	src/free.rs	-	prompt.txt
parallel	read	gemini-4-carbon-low	low	src/free.rs	parallel	prompt.txt
family	read	gemini-4-carbon-high	high	src/free.rs	family	prompt.txt
threshold	read	gemini-4-carbon-low	low	src/free.rs	threshold	prompt.txt
exhausted	read	gemini-4-carbon-low	low	src/free.rs	exhausted	prompt.txt
high	read	gemini-4-carbon-high	high	src/free.rs	limited	prompt.txt
xhigh	read	gemini-4-carbon-xhigh	xhigh	src/free.rs	limited	prompt.txt
fallback	read	gemini-4-carbon-low	low	src/free.rs	limited	prompt.txt
paused	read	gemini-4-carbon-low	low	src/free.rs	paused	prompt.txt
overlap	build	gemini-4-carbon-low	low	src/free.rs src/common.rs	overlap	prompt.txt
stale	build	gemini-4-carbon-low	low	src/free.rs	stale	prompt.txt
TASKS
else
  . "$HERE/env.sh"
  G=${GEM_DIR:-$SOMMELIER_RUNS/gemini}
  AG=${AGY_DIR:-$SOMMELIER_RUNS/agy}
  POLL=${POLL:-30}
  LIMIT_WAIT=${LIMIT_WAIT:-18060}
  TABLE=${TASKS:-$G/tasks.tsv}
  exec 3< "$TABLE" || exit 1
  STARTER=${GEM_STARTER:-$HERE/agy-agent.sh}
  MAX_BUILD=${MAX_BUILD:-6}
  LOG=$G/launcher.log; STATE=$G/state; RESULTS=$G/results.tsv; PIDF=$G/launcher.pid
  USAGE=$G/usage
  mkdir -p "$G" "$STATE" "$AG" "$G/logs" "$USAGE" || exit 1

  # One launcher at a time.
  if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF" 2>/dev/null)" 2>/dev/null; then
    echo "gemini-swarm: already running as $(cat "$PIDF")"; exit 1
  fi
  echo $$ > "$PIDF"; trap 'rm -f "$PIDF"' EXIT
fi

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >> "$LOG"; }

# ---- table ----
names=(); class=(); model=(); effort=(); scope=(); dep=(); prompt=(); excl=()
while IFS=$'\t' read -r a b c d e f g h || [ -n "${a:-}" ]; do
  case "${a:-}" in ''|'#'*) continue;; esac
  h=${h:-}; h=${h%$'\r'}; g=${g%$'\r'}
  [ "$b" = run ] && [ "$h" = exclusive ] && h=1 || h=0
  names+=("$a"); class+=("$b"); model+=("$c"); effort+=("$d"); scope+=("$e"); dep+=("${f:--}"); prompt+=("$g"); excl+=("$h")
done <&3
exec 3<&-
N=${#names[@]}
[ "$N" -gt 0 ] || { log "no tasks in $TABLE"; exit 1; }


idx() { local i; for ((i=0;i<N;i++)); do [ "${names[$i]}" = "$1" ] && { echo $i; return; }; done; echo -1; }
st() { cut -d' ' -f1 "$STATE/$1" 2>/dev/null || echo pending; }           # pending running done failed limit
stpid() { cut -d' ' -f2 "$STATE/$1" 2>/dev/null; }
setst() { local j; j=$(idx "$1"); echo "$2 ${3:-0} $(date +%s) ${class[$j]:-}" > "$STATE/$1"; }
alive() { [ -n "${1:-}" ] && [ "$1" != 0 ] && kill -0 "$1" 2>/dev/null; }
is_limit() {  # the worker's error file (or a tiny answer without READY/STUCK) shows a quota message
  local pat='RESOURCE_EXHAUSTED|[Ii]ndividual quota reached|quota (has been )?(reached|exceeded)|rate.?limit(ed)? exceeded|Too Many Requests'
  grep -a -q -E "$pat" "$AG/$1.err.txt" 2>/dev/null && return 0
  local l="$AG/$1.last.txt"
  if [ -f "$l" ] && [ "$(wc -c < "$l")" -lt 2000 ] && ! grep -a -q -E 'READY|STUCK' "$l"; then
    grep -a -q -E "$pat" "$l" && return 0
  fi
  return 1
}
good_answer() {  # the answer file is not empty and shows no quota or error marker
  local l="$AG/$1.last.txt"
  [ -s "$l" ] && ! grep -a -q -E 'RESOURCE_EXHAUSTED|[Qq]uota reached' "$l"
}
headline() {  # first READY or STUCK line of the answer, else its first line
  local l="$AG/$1.last.txt" h
  h=$(grep -a -m1 -E '^[[:space:]]*(READY|STUCK)' "$l" 2>/dev/null) || h=$(grep -a -m1 -E 'READY|STUCK' "$l" 2>/dev/null) || h=$(head -n1 "$l" 2>/dev/null)
  printf '%s' "$h" | tr '\t\r' '  ' | head -c 300
}
dep_ok() {  # dependency has ended (done or failed), not running and not waiting for a quota retry
  [ "$1" = - ] && return 0
  local j; j=$(idx "$1"); [ "$j" -ge 0 ] || { log "unknown dependency $1"; return 1; }
  case "$(st "$1")" in done|failed) return 0;; *) return 1;; esac
}
count_running() {  # class [only] -> number of running workers of that class (only=excl: exclusive run tasks only)
  local i n=0 p
  for ((i=0;i<N;i++)); do
    [ "${class[$i]}" = "$1" ] && [ "$(st "${names[$i]}")" = running ] || continue
    [ "${2:-}" = excl ] && [ "${excl[$i]}" != 1 ] && continue
    p=$(stpid "${names[$i]}")
    if alive "$p" || { [ -f "$AG/${names[$i]}.pid" ] && alive "$(cat "$AG/${names[$i]}.pid" 2>/dev/null)"; }; then n=$((n+1)); fi
  done
  echo $n
}
run_ready() {  # 0 when an exclusive run task could start now except for the builds
  local i; for ((i=0;i<N;i++)); do
    [ "${class[$i]}" = run ] && [ "${excl[$i]}" = 1 ] && [ "$(st "${names[$i]}")" = pending ] && dep_ok "${dep[$i]}" && resolve "$i" && return 0
  done; return 1
}

LIMITED_TSV=${G:-}/limited.tsv
declare -A LIM=()   # limited model family -> epoch when it may start again
fam() {  # model id -> family (the id without the effort suffix)
  case "$1" in gemini-pro-orchestrator*) echo gemini-pro-orchestrator;; *) echo "$1" | sed -E 's/-(low|medium|high|xhigh)$//';; esac
}
effword() { printf '%s' "$1" | sed -nE 's/.*-(low|medium|high|xhigh)$/\1/p'; }
next_fam() {  # fallback chain, empty means wait only
  case "$1" in
    claude-opus-5-5) echo gemini-4-neon;; gemini-4-neon) echo gemini-4-carbon;; gemini-4-carbon) echo gemini-4-hydrogen;;
    claude-sonnet-5-5) echo gemini-4-hydrogen;; gemini-4-hydrogen) echo gemini-4-argon;; gemini-4-argon) echo gemini-3.8-flash;;
  esac
}
efforts_of() {  # efforts a family has, lowest first (from agy models)
  case "$1" in gemini-4-neon|gemini-4-hydrogen) echo "low medium high xhigh";; *) echo "low medium high";; esac
}
save_limits() {  # limited.tsv (family, resume epoch, resume time) and the short summary limit.txt
  local f; : > "$LIMITED_TSV.tmp"
  for f in "${!LIM[@]}"; do printf '%s\t%s\t%s\n' "$f" "${LIM[$f]}" "$(date -d "@${LIM[$f]}" '+%F %T' 2>/dev/null)" >> "$LIMITED_TSV.tmp"; done
  mv -f "$LIMITED_TSV.tmp" "$LIMITED_TSV"
  if [ ${#LIM[@]} -eq 0 ]; then rm -f "$G/limit.txt"; else
    { echo "limited model families (details in limited.tsv):"; while IFS=$'\t' read -r a b c; do echo "$a  resume $c"; done < "$LIMITED_TSV"; } > "$G/limit.txt"
  fi
}
load_limits() {
  local a b c; [ -f "$LIMITED_TSV" ] || return 0
  while IFS=$'\t' read -r a b c; do [ -n "${a:-}" ] && [ "${b:-0}" -gt "$(date +%s)" ] 2>/dev/null && LIM[$a]=$b; done < "$LIMITED_TSV"
  save_limits
}
expire_limits() {  # families whose time has come may start again
  local f ch=0; for f in "${!LIM[@]}"; do
    [ "$(date +%s)" -ge "${LIM[$f]}" ] && { unset 'LIM[$f]'; ch=1; log "RESUME family $f may start again"; }
  done; [ "$ch" = 1 ] && save_limits; return 0
}
RMODEL=; REFF=
resolve() {  # task index -> RMODEL (model id to use) and REFF (effort argument), 1 when its whole chain is limited
  local i=$1 m=${model[$1]} f w cur nf ef e x
  f=$(fam "$m"); w=$(effword "$m"); [ -n "$w" ] || w=${effort[$i]}
  cur=$f
  while [ -n "${LIM[$cur]:-}" ]; do
    case "$w:${effort[$i]}" in
      high:*|xhigh:*|*:high|*:xhigh) return 1 ;;
    esac
    nf=$(next_fam "$cur"); [ -n "$nf" ] || return 1
    cur=$nf
  done
  if [ "$cur" = "$f" ]; then
    RMODEL=$m; REFF=${effort[$i]}
    case "$m" in *-low|*-medium|*-high|*-xhigh|gemini-pro-orchestrator*) REFF=;; esac
  else
    ef=$(efforts_of "$cur"); e=${ef##* }
    for x in $ef; do [ "$x" = "$w" ] && e=$w; done
    RMODEL=$cur-$e; REFF=
  fi
  return 0
}
usedmodel() { cat "$STATE/$1.model" 2>/dev/null || echo "${model[$(idx "$1")]}"; }

# Count elapsed seconds inside today's window, including time before a restart.
usage_today() {
  local now=$1 midnight file begin end total=0 n
  midnight=$(date -d "$(date -d "@$now" +%F)" +%s) || return 1
  for file in "$USAGE"/*; do
    [ -f "$file" ] || continue
    read -r begin end < "$file" || return 1
    (( begin < midnight )) && begin=$midnight
    (( end > begin )) && total=$((total + end - begin))
  done
  for n in "${names[@]}"; do
    [ "$(st "$n")" = running ] || continue
    read -r _ _ begin _ < "$STATE/$n" || return 1
    [ -f "$USAGE/$n.$begin" ] && continue
    (( begin < midnight )) && begin=$midnight
    (( now > begin )) && total=$((total + now - begin))
  done
  printf '%s\n' "$total"
}

record_usage() {
  local n=$1 begin now file
  read -r _ _ begin _ < "$STATE/$n" || return 1
  now=$(date +%s)
  file=$USAGE/$n.$begin
  [ -f "$file" ] && return 0
  printf '%s %s\n' "$begin" "$now" > "$USAGE/.pending" &&
    mv -f "$USAGE/.pending" "$file"
}

running_snapshot() {
  local i n
  running_models=(); running_scopes=()
  for ((i=0;i<N;i++)); do
    n=${names[$i]}
    [ "$(st "$n")" = running ] || continue
    running_models+=("$(usedmodel "$n")")
    # Read tasks write no files, their scope is free text.
    [ "${class[$i]}" = read ] && running_scopes+=("") || running_scopes+=("${scope[$i]}")
  done
}

start_allowed() {
  local i=$1 now=$2 seconds=$3 fresh=$4 fm running count=0 a b
  local -a requested active
  reason=
  if (( now < pause_until )); then
    reason="quota pause"
  elif (( seconds >= DAILY_BUDGET_MIN * 60 )); then
    reason="daily budget used up"
  elif (( seconds * 100 >= DAILY_BUDGET_MIN * 60 * BUDGET_STOP_PCT )); then
    reason="budget stop at $BUDGET_STOP_PCT percent"
  elif (( ${#running_models[@]} >= MAX_PARALLEL )); then
    reason="parallel cap"
  elif ! resolve "$i"; then
    case "${effort[$i]}:$(effword "${model[$i]}")" in
      high:*|xhigh:*|*:high|*:xhigh) reason="high effort waits for its own family" ;;
      *) reason="model families limited" ;;
    esac
  else
    fm=$(fam "$RMODEL")
    for running in "${running_models[@]}"; do
      [ "$(fam "$running")" = "$fm" ] && count=$((count + 1))
    done
    if (( count >= MAX_PER_FAMILY )); then
      reason="family cap"
    else
      requested=()
      [ "${class[$i]}" = read ] || IFS=$' \t' read -r -a requested <<< "${scope[$i]}"
      for running in "${running_scopes[@]}"; do
        IFS=$' \t' read -r -a active <<< "$running"
        for a in "${requested[@]}"; do
          for b in "${active[@]}"; do
            if [ "$a" = "$b" ]; then reason="file overlap ($a)"; return 1; fi
          done
        done
      done
      if [ "${class[$i]}" = build ] && [ "$fresh" != yes ]; then
        reason="stale base"
      fi
    fi
  fi
  [ -z "$reason" ]
}

if [ "$DRY" = 1 ]; then
  # Give each fixture row an independent scheduler snapshot.
  for ((i=0;i<N;i++)); do
    running_models=(); running_scopes=(); LIM=(); pause_until=0
    seconds=0; fresh=yes
    case "${dep[$i]}" in
      parallel) running_models=(fixture-a fixture-b fixture-c fixture-d) ;;
      family) running_models=(gemini-4-carbon-low gemini-4-carbon-medium) ;;
      threshold) seconds=$((DAILY_BUDGET_MIN * 60 * BUDGET_STOP_PCT / 100)) ;;
      exhausted) seconds=$((DAILY_BUDGET_MIN * 60)) ;;
      limited) LIM[gemini-4-carbon]=2000 ;;
      paused) pause_until=2000 ;;
      overlap) running_scopes=("src/common.rs src/active.rs"); running_models=(fixture-a) ;;
      stale) fresh=no ;;
    esac
    if start_allowed "$i" 1000 "$seconds" "$fresh"; then
      printf '%s: start (%s)\n' "${names[$i]}" "$RMODEL"
    else
      printf '%s: wait (%s)\n' "${names[$i]}" "$reason"
    fi
  done
  exit 0
fi

start_task() {
  local i=$1 n=${names[$1]} folder branch eff m now seconds fresh=yes
  folder=sommelier-agy-$n; branch=gemini/$n
  if [ ! -d "$SOMMELIER_REPOS/$folder" ]; then
    if ! sh "$HERE/wt.sh" "$folder" "$branch" >> "$G/logs/$n.wt.txt" 2>&1; then
      # The branch may exist from an earlier try.
      ( cd "$SOMMELIER_CLONE" && git worktree add "$SOMMELIER_REPOS/$folder" "$branch" ) >> "$G/logs/$n.wt.txt" 2>&1 \
        || { log "FAIL $n: no worktree (see logs/$n.wt.txt)"; setst "$n" failed 0; result "$n" failed "-" "$branch" "no worktree"; return 1; }
    fi
  fi
  if [ "${class[$i]}" = build ]; then
    # A worker branch without own commits moves forward by itself. One with own commits waits for a rebase.
    if ! (
      cd "$SOMMELIER_REPOS/$folder" &&
      git fetch --quiet "$SOMMELIER_UPSTREAM" main &&
      { git merge-base --is-ancestor FETCH_HEAD HEAD || git merge --quiet --ff-only FETCH_HEAD; }
    ) >> "$G/logs/$n.wt.txt" 2>&1; then
      fresh=no
      log "WAIT $n: stale or unverified base (see logs/$n.wt.txt)"
    fi
  fi
  # Preparation may take time. Check for quota messages and budget again.
  observe_quota
  now=$(date +%s)
  seconds=$(usage_today "$now") || return 1
  start_allowed "$i" "$now" "$seconds" "$fresh" || return 1
  m=$RMODEL
  rm -f "$AG/$n.done" "$AG/$n.done.reported" "$AG/$n.last.txt" "$AG/$n.err.txt" "$STATE/$n.quota"
  eff=$REFF
  [ "$m" = "${model[$i]}" ] || log "FALLBACK $n ${model[$i]} -> $m"
  echo "$m" > "$STATE/$n.model"
  AGY_EFFORT=$eff bash "$STARTER" "$n" "$folder" "$branch" "${prompt[$i]}" "$m" > "$G/logs/$n.starter.txt" 2>&1 < /dev/null &
  local pid=$!
  setst "$n" running "$pid"
  log "START $n class ${class[$i]} model $m effort ${eff:-id} branch $branch pid $pid"
}

result() {  # name state exit branch headline [model]
  [ -f "$RESULTS" ] || printf 'time\tname\tstate\texit\tbranch\tlast message\theadline\tmodel\n' > "$RESULTS"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$1" "$2" "$3" "$4" "$AG/$1.last.txt" "$5" "${6:-$(usedmodel "$1")}" >> "$RESULTS"
}

PAUSE_FILE=$G/pause-until
pause_until=0

pause_for_quota() {
  local n=$1 now until
  [ -f "$STATE/$n.quota" ] && return 0
  now=$(date +%s)
  until=$((now + 1800))
  printf '%s\n' "$until" > "$PAUSE_FILE.tmp" && mv -f "$PAUSE_FILE.tmp" "$PAUSE_FILE" || exit 1
  pause_until=$until
  printf '%s\n' "$now" > "$STATE/$n.quota" || exit 1
  log "PAUSE $n: quota message, all starts paused for 30 minutes"
}

observe_quota() {
  local n
  for n in "${names[@]}"; do
    [ "$(st "$n")" = running ] && [ ! -f "$STATE/$n.quota" ] && is_limit "$n" || continue
    pause_for_quota "$n"
  done
}

quota_hit() {  # Limit the family and return the task to pending.
  local n=$1 code=$2 m fm until seen
  pause_for_quota "$n"
  read -r seen < "$STATE/$n.quota" || exit 1
  m=$(usedmodel "$n"); fm=$(fam "$m"); until=$((seen + LIMIT_WAIT))
  (( until < ${LIM[$fm]:-0} )) && until=${LIM[$fm]}
  LIM[$fm]=$until; save_limits
  rm -f "$STATE/$n" "$STATE/$n.model"
  log "LIMIT $n hit a quota message on $m, family $fm limited until $(date -d "@$until" '+%T' 2>/dev/null)"
  result "$n" limit "$code" "gemini/$n" "quota message" "$m"
}

# A task left in the old state "limit" waits again, the family limits decide which model it gets.
for sf in "$STATE"/*; do [ -f "$sf" ] && [ "$(cut -d' ' -f1 "$sf" 2>/dev/null)" = limit ] && { rm -f "$sf"; log "RECOVER $(basename "$sf") was in state limit, pending again"; }; done
load_limits
if [ -f "$PAUSE_FILE" ]; then
  read -r pause_until < "$PAUSE_FILE" || exit 1
fi
log "launcher $$ started, $N tasks from $TABLE, max build $MAX_BUILD"

while :; do
  observe_quota
  # ---- reap ----
  for ((i=0;i<N;i++)); do
    n=${names[$i]}; [ "$(st "$n")" = running ] || continue
    p=$(stpid "$n")
    if [ -f "$AG/$n.done" ]; then
      record_usage "$n" || exit 1
      read -r code head < "$AG/$n.done" || true
      if [ -f "$STATE/$n.quota" ] || is_limit "$n"; then
        rm -f "$AG/$n.done"; quota_hit "$n" "${code:-?}"
      else
        setst "$n" done 0; log "DONE $n exit ${code:-?} ${head:-}"
        result "$n" done "${code:-?}" "gemini/$n" "${head:-}"
      fi
    elif ! alive "$p" && ! { [ -f "$AG/$n.pid" ] && alive "$(cat "$AG/$n.pid" 2>/dev/null)"; }; then
      # The process ended and the .done marker is gone. A watcher may have moved it to .done.reported,
      # so judge by the answer file. Only an empty or missing answer counts as lost.
      record_usage "$n" || exit 1
      code=; [ -f "$AG/$n.done.reported" ] && read -r code _ < "$AG/$n.done.reported"
      case "${code:-}" in ''|*[!0-9]*) code='?';; esac
      if [ -f "$STATE/$n.quota" ] || is_limit "$n"; then
        quota_hit "$n" "$code"
      elif good_answer "$n"; then
        head=$(headline "$n")
        setst "$n" done 0; log "DONE $n exit $code (no .done marker, read the answer) $head"
        result "$n" done "$code" "gemini/$n" "$head"
      else
        echo "lost " > "$AG/$n.done"; setst "$n" failed 0; log "FAIL $n: process ended, no answer and no .done marker"
        result "$n" failed lost "gemini/$n" "worker lost"
      fi
    fi
  done
  # ---- all finished? ----
  open=0; for ((i=0;i<N;i++)); do case "$(st "${names[$i]}")" in done|failed) ;; *) open=$((open+1));; esac; done
  [ "$open" -eq 0 ] && break
  # ---- starts ----
  expire_limits
  for ((i=0;i<N;i++)); do
    n=${names[$i]}; [ "$(st "$n")" = pending ] || continue
    dep_ok "${dep[$i]}" || continue
    observe_quota
    now=$(date +%s)
    seconds=$(usage_today "$now") || exit 1
    running_snapshot
    start_allowed "$i" "$now" "$seconds" yes || continue
    case "${class[$i]}" in
      read) start_task "$i" ;;
      build)
        [ "$(count_running run excl)" -eq 0 ] || continue
        run_ready && continue
        [ "$(count_running build)" -lt "$MAX_BUILD" ] || continue
        start_task "$i" ;;
      run)
        [ "$(count_running run)" -eq 0 ] || continue
        [ "${excl[$i]}" != 1 ] || [ "$(count_running build)" -eq 0 ] || continue
        start_task "$i" ;;
      *) log "FAIL $n: unknown class ${class[$i]}"; setst "$n" failed 0 ;;
    esac
  done
  # ---- the rules, checked every round ----
  nb=$(count_running build); nr=$(count_running run)
  [ "$nb" -le "$MAX_BUILD" ] || log "VIOLATION $nb builds running"
  ne=$(count_running run excl)
  { [ "$nr" -gt 1 ] || { [ "$ne" -ge 1 ] && [ "$nb" -ge 1 ]; }; } && log "VIOLATION run $nr (exclusive $ne) with builds $nb"
  sleep "$POLL"
done
log "all $N tasks finished"
exit 0
