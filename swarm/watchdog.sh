#!/usr/bin/env bash
# Watchdog over everything that can hang, for all kinds of agents (Claude, Codex, Antigravity,
# plain scripts). No model is involved: it looks at files and processes once a minute.
#
#   watchdog.sh            runs until it finds something a person or the coordinator must decide,
#                          prints that as one line and ends (so whoever started it is woken)
#   watchdog.sh --once     one pass, prints what it sees, for a look by hand
#
# What it repairs itself (and writes to $SOMMELIER_RUNS/watchdog.log):
#   - a screenshot step that hangs longer than 40 s is killed
#   - a game lock whose owner process is gone is removed, the title it left is stopped
#   - a title that runs with no lock at all for a minute is stopped
#   - queue tickets and agent markers of processes that are gone are removed, the agent's row on
#     the bus is set to idle
# What it reports and ends on (each thing once):
#   - a game run that holds the lock more than 4 minutes past its own time
#   - a run that waits in the queue longer than 20 minutes
#   - a landing run (integration lock) whose log has not moved for 8 minutes
#   - work handed out on the bus that nobody answered, that is late or was declined,
#     participants with nothing to do and participants that talk but publish no fresh status rows
#     (bus-ledger.py)
#   - a participant whose quota is back (rows of $SOMMELIER_QUOTAS, one minute after the time)
#   - a Codex or Antigravity agent that finished (with its report line)
#   - a Codex or Antigravity agent that is alive and silent for 20 minutes
#   - the bus not answering twice in a row
# WATCHDOG_FOREVER=1 keeps the loop running and appends the findings to $SOMMELIER_RUNS/watchdog.findings
# instead of ending on them. Every pass resends the roster $SOMMELIER_ROSTER to the bus.
# Settings: swarm/env.sh. The bus layers run only when SOMMELIER_URL is set. Starting the hub
# and the tunnel again needs SOMMELIER_HUB_HOST=1 (and SOMMELIER_TUNNEL for the tunnel).
set -u
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
RUNS=$SOMMELIER_RUNS; LOCK=$RUNS/.lock; QUEUE=$RUNS/.queue
SW=$SOMMELIER_STATE
mkdir -p "$RUNS"
LOGF=$RUNS/watchdog.log; SEEN=$RUNS/.watchdog.seen
ONCE=0; [ "${1:-}" = "--once" ] && ONCE=1
touch "$SEEN"
now() { date +%s; }
age() { echo $(( $(now) - $(stat -c %Y "$1" 2>/dev/null || now) )); }
note() { echo "$(date '+%F %T') $*" >> "$LOGF"; [ $ONCE = 1 ] && echo "repaired: $*"; }
BUS() { [ -n "$SOMMELIER_URL" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 15 sommelier "$@" >/dev/null 2>&1; }
TITLE_PROC=${SOMMELIER_TITLE_PROCESS:-${SOMMELIER_TITLE_EXE:+$(basename "$SOMMELIER_TITLE_EXE")}}
title_runs() { [ -n "$TITLE_PROC" ] && tasklist //FI "IMAGENAME eq $TITLE_PROC" 2>/dev/null | grep -qi -F "$TITLE_PROC"; }
stop_title() { [ -n "$TITLE_PROC" ] && taskkill //F //IM "$TITLE_PROC" >/dev/null 2>&1; }
findings=""
found() {  # key, text: report each key once
  if ! grep -qxF "$1" "$SEEN"; then echo "$1" >> "$SEEN"; findings="$findings
WATCHDOG $2"; fi
  [ $ONCE = 1 ] && echo "finding: $2"
}

bus_misses=0; nolock_since=0
while :; do
  findings=""
  # 1. A hanging screenshot step.
  powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='powershell.exe'\" | Where-Object { \$_.CommandLine -like '*shot.ps1*' -and ((Get-Date) - \$_.CreationDate).TotalSeconds -gt 40 } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force; \$_.ProcessId }" 2>/dev/null | while read -r p; do [ -n "$p" ] && note "killed a screenshot step that hung (process $p)"; done

  # 2. The game lock.
  if [ -d "$LOCK" ]; then
    nolock_since=0
    pid=$(cat "$LOCK/pid" 2>/dev/null); label=$(cat "$LOCK/label" 2>/dev/null); secs=$(cat "$LOCK/secs" 2>/dev/null)
    a=$(age "$LOCK")
    if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
      rm -rf "$LOCK"; title_runs && stop_title
      note "removed the game lock of '$label': its owner process $pid is gone"
    elif [ -z "$pid" ] && [ "$a" -gt 60 ]; then
      found "lock-nopid-$label" "the game lock '$label' has no owner recorded for $a s"
    elif [ -n "$secs" ] && [ "$a" -gt $((secs + 240)) ]; then
      found "lock-over-$label-$pid" "the game run '$label' holds the lock for $a s, its own time was $secs s"
    elif [ -z "$secs" ] && [ "$a" -gt 2700 ]; then
      found "lock-long-$label-$pid" "'$label' holds the game lock for $((a / 60)) minutes"
    fi
  elif title_runs; then
    [ $nolock_since = 0 ] && nolock_since=$(now)
    if [ $(( $(now) - nolock_since )) -gt 60 ]; then stop_title; note "stopped a title that ran with no game lock"; nolock_since=0; fi
  else nolock_since=0
  fi

  # 3. The queue of waiting runs.
  for t in "$QUEUE"/*; do
    [ -f "$t" ] || continue
    p=$(basename "$t"); read -r tl ts tsecs < "$t"
    if ! kill -0 "$p" 2>/dev/null; then rm -f "$t"; note "removed the queue ticket of '$tl': its process is gone"; continue; fi
    w=$(( $(now) - ${ts:-$(now)} ))
    [ "$w" -gt 1200 ] && found "queue-$p" "the run '$tl' waits in the queue for $((w / 60)) minutes (lock: $(cat "$LOCK/label" 2>/dev/null))"
  done

  # 4. A landing run that stands still.
  if [ -d "$SW/.integrate.lock" ]; then
    l=$(ls -t "$SW"/logs/* 2>/dev/null | head -1)
    if [ -n "$l" ] && [ "$(age "$l")" -gt 480 ] && [ ! -d "$LOCK" -o "$(cat "$LOCK/label" 2>/dev/null)" = integration-tests ]; then
      found "landing-$(basename "$l")-$(stat -c %Y "$l")" "the landing run is silent for $(( $(age "$l") / 60 )) minutes, last line: $(tail -1 "$l" | cut -c1-140)"
    fi
  fi

  # 5. Codex, Claude, and Antigravity agents: verify OS process liveness automatically.
  HOST_NAME=${SOMMELIER_MACHINE:-$(hostname 2>/dev/null || echo local)}
  for kind in codex claude agy; do
    for pf in "$RUNS/$kind"/*.pid; do
      [ -f "$pf" ] || continue
      n=$(basename "$pf" .pid); p=$(cat "$pf")
      if ! kill -0 "$p" 2>/dev/null; then
        rm -f "$pf"
        if [ -f "$SOMMELIER_ROSTER" ]; then
          { grep -v -P "^$kind-$n\t" "$SOMMELIER_ROSTER" 2>/dev/null; printf '%s\t%s\t%s\t%s\t%s\n' "$kind-$n" "idle" "$kind" "" "ended without a report"; } > "$SOMMELIER_ROSTER.wd.$$" && mv "$SOMMELIER_ROSTER.wd.$$" "$SOMMELIER_ROSTER"
        fi
        BUS status set "$kind-$n" --state idle --host "$HOST_NAME" "ended without a report, see $RUNS/$kind/$n.last.txt"
        note "$kind agent '$n' is gone without cleaning up, its row is set to idle"
        found "agent-gone-$kind-$n-$p" "the $kind agent '$n' ended without a report"
        continue
      fi
      act="$RUNS/$kind/$n.events.jsonl"
      [ "$kind" = agy ] && act="$RUNS/$kind/$n.log.txt"
      [ "$kind" = claude ] && act="$RUNS/$kind/$n.last.txt"
      s=$(age "$act")
      [ "$s" -gt 1200 ] && found "agent-silent-$kind-$n-$(stat -c %Y "$act" 2>/dev/null)" "the $kind agent '$n' is alive and silent for $((s / 60)) minutes"
    done
  done

  # 5b. An agent that finished: its report line wakes the coordinator, once.
  for kind in codex claude agy; do
    for df in "$RUNS/$kind"/*.done; do
      [ -f "$df" ] || continue
      n=$(basename "$df" .done)
      found "agent-done-$kind-$n-$(stat -c %Y "$df")" "the $kind agent '$n' finished (exit and report line): $(head -c 320 "$df" | tr '
' ' ') : full report in $RUNS/$kind/$n.last.txt"
      mv "$df" "$df.reported"
    done
  done

  # 5c. Probe supervised `sommelier agent` children via OS PIDs so heartbeats stay OS-verified.
  [ -n "$SOMMELIER_URL" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 20 sommelier agent watch --once >/dev/null 2>&1

  # 6. The bus, in four layers: the hub on this PC, the tunnel process, the public address end to
  #    end, and the coordinator's own listener. A dead hub or tunnel is started again at once.
  BUSDATA=$SOMMELIER_DATA
  if [ "$SOMMELIER_HUB_HOST" = 1 ] && [ "$(curl -s -o /dev/null -m 5 -w '%{http_code}' "$SOMMELIER_LOCAL/" 2>/dev/null)" != 200 ]; then
    if ! tasklist 2>/dev/null | grep -qi '^sommelier.exe'; then
      ( cd "$BUSDATA" && nohup ./bin/sommelier.exe serve --ui "$SOMMELIER_UI" >> "$BUSDATA/hub.out.txt" 2>&1 & )
      sleep 4; note "the hub was down and is started again"
      BUS send CHAT "hub restarted at $(date -u '+%H:%M') UTC, messages sent while it was down are lost, send them again" --topic status
      found "hub-restart-$(now)" "the hub was down, the watchdog started it again (check $RUNS/watchdog.log and $BUSDATA/hub.out.txt)"
    else found "hub-deaf-$(date +%H%M)" "a sommelier process runs but the hub does not answer on $SOMMELIER_LOCAL"
    fi
  fi
  if [ "$SOMMELIER_HUB_HOST" = 1 ] && [ -n "$SOMMELIER_TUNNEL" ] && ! tasklist 2>/dev/null | grep -qi '^cloudflared.exe'; then
    ( cd "$HOME" && nohup cloudflared tunnel run "$SOMMELIER_TUNNEL" >> "$BUSDATA/cloudflared.log" 2>&1 & )
    sleep 6; note "the tunnel was down and is started again"
    found "tunnel-restart-$(now)" "the Cloudflare tunnel was down, the watchdog started it again"
  fi
  if [ -z "$SOMMELIER_URL" ]; then bus_misses=0
  elif [ "$(curl -s -o /dev/null -m 15 -w '%{http_code}' "https://${SOMMELIER_URL#*://}/" 2>/dev/null)" = 200 ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 20 sommelier who >/dev/null 2>&1; then bus_misses=0
  else bus_misses=$((bus_misses + 1)); [ $bus_misses -ge 2 ] && found "bus-$(date +%H%M)" "the bus does not answer at its public address (page or WebSocket), hub and tunnel processes are there: look at the tunnel"
  fi
  # The coordinator hears the bus through one waiting `sommelier tail`. Without it it is deaf.
  if [ -n "$SOMMELIER_URL" ] && ! powershell -NoProfile -Command "if (Get-CimInstance Win32_Process -Filter \"Name='sommelier.exe'\" | Where-Object { \$_.CommandLine -like '* tail*' }) { exit 0 } else { exit 1 }" 2>/dev/null; then
    deaf=$((${deaf:-0} + 1)); [ $deaf -ge 2 ] && { found "listener-$(date +%H%M)" "no bus listener of the coordinator is running: start the wait on the bus again"; deaf=0; }
  else deaf=0
  fi

  # 6b. A participant whose quota comes back: nudge it one minute after the time it named.
  #     $SOMMELIER_QUOTAS is tab separated (who, what, gone, back, note) with a header line.
  if [ -f "$SOMMELIER_QUOTAS" ]; then
    tail -n +2 "$SOMMELIER_QUOTAS" | while IFS="$(printf '	')" read -r who what gone back note; do
      t=$(date -u -d "$back" +%s 2>/dev/null) || continue
      [ "$(now)" -ge $((t + 60)) ] && [ "$(now)" -lt $((t + 7200)) ] || continue
      echo "$who|$what|$back"
    done > "$RUNS/.quota.due"
    while IFS='|' read -r who what back; do
      [ -n "$who" ] && found "quota-back-$who-$back" "quota of $who ($what) is back since $back UTC: send the nudge now (full briefing: MAIN, YOURS, NEXT, ALSO FREE, HOW)"
    done < "$RUNS/.quota.due"
  fi

  # 6c. Before syncing the roster, never resurrect a launcher-managed worker whose OS PID is gone.
  if [ -f "$SOMMELIER_ROSTER" ]; then
    while IFS="$(printf '\t')" read -r r_agent r_state r_model r_branch r_text; do
      case "$r_agent" in
        codex-*|claude-*|agy-*)
          if [ "$r_state" = "running" ]; then
            r_kind=${r_agent%%-*}; r_name=${r_agent#*-}
            r_pf="$RUNS/$r_kind/$r_name.pid"
            if [ ! -f "$r_pf" ] || ! kill -0 "$(cat "$r_pf" 2>/dev/null)" 2>/dev/null; then
              r_state="idle"
              r_text="process not running (reconciled by watchdog)"
            fi
          fi
          ;;
      esac
      printf '%s\t%s\t%s\t%s\t%s\n' "$r_agent" "$r_state" "$r_model" "$r_branch" "$r_text"
    done < "$SOMMELIER_ROSTER" > "$SOMMELIER_ROSTER.clean.$$" && mv "$SOMMELIER_ROSTER.clean.$$" "$SOMMELIER_ROSTER"
  fi
  [ -n "$SOMMELIER_URL" ] && [ -f "$SOMMELIER_ROSTER" ] && SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 20 sommelier status sync --file "$SOMMELIER_ROSTER" >/dev/null 2>&1

  # 7. The work handed out on the bus, every third pass: offers nobody answered, accepted work
  #    that is late, declined offers, and participants with nothing to do.
  pass=$((${pass:-0} + 1))
  if [ -n "$SOMMELIER_URL" ] && [ $((pass % 3)) = 1 ]; then
    SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 30 sommelier log --since 1 --limit 1000 --json 2>/dev/null > "$RUNS/.buslog.json"
    SOMMELIER_TOKEN_FILE="$SOMMELIER_ORCH_TOKEN" timeout 30 sommelier status list --json 2>/dev/null > "$RUNS/.busstatus.json"
    BUS_STATUS="$RUNS/.busstatus.json" python -I "$HERE/bus-ledger.py" < "$RUNS/.buslog.json" 2>/dev/null | while IFS= read -r l; do
      [ -n "$l" ] && echo "$l"
    done > "$RUNS/.ledger.out"
    while IFS= read -r l; do
      case "$l" in
        POLL*)  # a silent participant: poll it directly on the bus, once per 15 minute slot, no wake up
          set -- $l; who=$2; slot=$3; mins=${5:-15}
          if ! grep -qxF "poll-$who-$slot" "$SEEN"; then
            echo "poll-$who-$slot" >> "$SEEN"
            BUS send CHAT "$who: silent for $mins min, answer with a fresh status row" --topic status --to "$who"
            note "polled $who on the bus after $mins minutes of silence"
          fi ;;
        *) k=$(printf '%s' "$l" | cut -d' ' -f2); found "ledger-$k" "bus work: $(printf '%s' "$l" | cut -d' ' -f1) $(printf '%s' "$l" | cut -d: -f2- | cut -c1-360)" ;;
      esac
    done < "$RUNS/.ledger.out"
  fi

  if [ -n "$findings" ]; then
    echo "$(date '+%F %T') reported:$findings" >> "$LOGF"
    if [ "${WATCHDOG_FOREVER:-0}" = 1 ]; then
      # Keep watching: findings go to a file, a separate small waiter wakes the coordinator.
      echo "$findings" | sed '/^$/d' >> "$RUNS/watchdog.findings"
    else
      echo "$findings" | sed '/^$/d'; [ $ONCE = 0 ] && exit 0
    fi
  fi
  [ $ONCE = 1 ] && { echo "pass done: lock '$(cat "$LOCK/label" 2>/dev/null)' age $( [ -d "$LOCK" ] && age "$LOCK" || echo 0 ) s, $(ls "$QUEUE" 2>/dev/null | wc -l) waiting, landing lock $( [ -d "$SW/.integrate.lock" ] && echo held || echo free )"; exit 0; }
  sleep 60
done
