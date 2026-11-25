#!/bin/sh
# Checks the long lived swarm loops once a minute and restarts a loop that has stopped.
# Process detection uses Windows process command lines because these loops have no loop pid
# files. The lander batch lock is checked separately so a batch is never interrupted.
HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
RUNS=$SOMMELIER_RUNS; SW=$SOMMELIER_STATE; FIND=$RUNS/watchdog.findings
if [ "${GUARDIAN_DRY:-0}" != 1 ]; then
  mkdir -p "$RUNS" "$SW/logs"
  touch "$FIND" 2>/dev/null || :
fi

assumed_dead() {
  case ",${GUARDIAN_ASSUME_DEAD:-}," in *",$1,"*) return 0 ;; esac
  return 1
}

loop_alive() {
  name=$1
  assumed_dead "$name" && return 1
  powershell.exe -NoProfile -NonInteractive -Command '$n=$args[0]; $needle="swarm/$n.sh"; $me=$PID; $found=Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -and $_.CommandLine.Contains($needle) }; if ($found) { exit 0 } else { exit 1 }' "$name" >/dev/null 2>&1
}

lander_batch_live() {
  pidfile=$SW/.integrate.lock/pid
  [ -f "$pidfile" ] || return 1
  pid=$(cat "$pidfile" 2>/dev/null)
  case "$pid" in ''|*[!0-9]*) return 1 ;; esac
  powershell.exe -NoProfile -NonInteractive -Command '$p=[int]$args[0]; $found=Get-CimInstance Win32_Process -Filter "ProcessId = $p" -ErrorAction SilentlyContinue; if ($found) { exit 0 } else { exit 1 }' "$pid" >/dev/null 2>&1
}

launch() {
  name=$1; command=$2
  if [ "${GUARDIAN_DRY:-0}" = 1 ]; then
    printf '%s\n' "$command"
    return
  fi
  sh -c "$command" >>"$SW/logs/guardian-$name.log" 2>&1 </dev/null &
  stamp=$(date '+%F %T')
  printf 'GUARDIAN %s restarted %s, it was not running (%s)\n' "$stamp" "$name" "$command" >> "$FIND" 2>/dev/null || :
}

round() {
  if ! loop_alive watchdog; then
    launch watchdog 'WATCHDOG_FOREVER=1 sh swarm/watchdog.sh'
  fi
  if ! loop_alive lander; then
    if lander_batch_live; then
      if [ "${GUARDIAN_DRY:-0}" = 1 ]; then
        printf '%s\n' 'lander restart skipped because a batch runs'
      else
        printf 'GUARDIAN %s skipped lander restart because a batch runs\n' "$(date '+%F %T')" >> "$FIND" 2>/dev/null || :
      fi
    else
      launch lander 'sh swarm/lander.sh'
    fi
  fi
  if ! loop_alive bus-listen; then
    launch bus-listen 'sh swarm/bus-listen.sh'
  fi
}

while :; do
  round
  [ "${GUARDIAN_ONCE:-0}" = 1 ] && exit 0
  sleep 60
done
