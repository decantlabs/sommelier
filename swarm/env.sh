# Settings of the swarm harness. Every script of this folder sources this file:
#
#   HERE=$(cd "$(dirname "$0")" && pwd); . "$HERE/env.sh"
#
# Every setting is read from the environment first, so put your own values in your shell profile
# (or in the environment of the process that starts the script). Paths are written with forward
# slashes, a Windows drive as C:/folder, because the scripts run under Git Bash and hand the same
# text to Windows programs.
#
#   SOMMELIER_REPOS     the folder that holds the clone and all worktrees        parent of this checkout
#   SOMMELIER_CLONE     the main clone of the project, worktrees are made from it   $SOMMELIER_REPOS/decant
#   SOMMELIER_INTEGRATE a clean checkout used only by integrate.sh               $SOMMELIER_REPOS/sommelier-integrate
#   SOMMELIER_RUNS      run results, the title lock, the queue, agent markers    $SOMMELIER_REPOS/sommelier-runs
#   SOMMELIER_STATE     the landing lock and landing logs                        $SOMMELIER_RUNS/swarm
#   SOMMELIER_TITLE_EXE the executable of the title that run-title.sh starts     no default, run-title.sh needs it
#   SOMMELIER_TITLE_ARGS, SOMMELIER_TITLE_DIR, SOMMELIER_TITLE_PROCESS, SOMMELIER_TITLE_LOG
#                    optional details of the title, see run-title.sh
#   SOMMELIER_GPU_CRATES  space separated crates whose tests need the GPU (the lander runs them under the run lock)
#   SOMMELIER_TITLE_BUILD  optional shell command that builds the title in a worktree before a landing run
#   SOMMELIER_TITLE_RUN_ARGS  optional VAR=value words handed to run-title.sh by the lander
#   SOMMELIER_PRIORART  optional list of folders with related projects to read first      none
#   SOMMELIER_UPSTREAM  git remote of the shared repository                      upstream
#   SOMMELIER_FORK      git remote the workers push their branches to            origin
#   SOMMELIER_ORCH      the orchestrator's name on the bus                       owner
#   SOMMELIER_URL       the hub, as wss://host                                   none, bus steps are skipped
#   SOMMELIER_DATA      data folder of the bus on this PC                        the sommelier default
#   SOMMELIER_ORCH_TOKEN  file with the orchestrator's token (not exported)      $SOMMELIER_DATA/orchestrator.token
#   SOMMELIER_HUB_HOST  1 when the hub runs on this PC and the watchdog may start it   0
#   SOMMELIER_LOCAL     the hub's local address, used when SOMMELIER_HUB_HOST=1     http://127.0.0.1:7300
#   SOMMELIER_UI        the dashboard folder of the hub                          <this checkout>/bus/ui
#   SOMMELIER_TUNNEL    name of a cloudflared tunnel the watchdog keeps up       none
#   SOMMELIER_BIN       the sommelier program bus-listen.sh runs                    $SOMMELIER_DATA/bin/sommelier.exe if it exists, else sommelier
#   SOMMELIER_ROSTER    roster file of the orchestrator's own agent rows         $SOMMELIER_DATA/roster.tsv
#   SOMMELIER_QUOTAS    quota return times, tab separated: who, what, gone, back, note   $SOMMELIER_DATA/quotas.tsv
#
# No token is stored here. SOMMELIER_ORCH_TOKEN only names the file, and it is not exported: the scripts
# hand it to the `sommelier` client for their own calls, so the agents they start never inherit it.

_sw_here=${HERE:-$(cd "$(dirname "$0")" && pwd)}
_sw_m() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
_sw_top=$(_sw_m "$(cd "$_sw_here/.." && pwd)")

SOMMELIER_REPOS=$(_sw_m "${SOMMELIER_REPOS:-$(dirname "$_sw_top")}")
: "${SOMMELIER_CLONE:=$SOMMELIER_REPOS/decant}"
: "${SOMMELIER_INTEGRATE:=$SOMMELIER_REPOS/sommelier-integrate}"
: "${SOMMELIER_RUNS:=$SOMMELIER_REPOS/sommelier-runs}"
: "${SOMMELIER_STATE:=$SOMMELIER_RUNS/swarm}"
: "${SOMMELIER_TITLE_EXE:=}" "${SOMMELIER_TITLE_ARGS:=}" "${SOMMELIER_TITLE_DIR:=}" "${SOMMELIER_TITLE_PROCESS:=}" "${SOMMELIER_TITLE_LOG:=}"
: "${SOMMELIER_TITLE_BUILD:=}" "${SOMMELIER_TITLE_RUN_ARGS:=}"
: "${SOMMELIER_UPSTREAM:=upstream}" "${SOMMELIER_FORK:=origin}"
: "${SOMMELIER_ORCH:=owner}"
: "${SOMMELIER_URL:=}"
: "${SOMMELIER_DATA:=${LOCALAPPDATA:-${XDG_DATA_HOME:-$HOME/.local/share}}/sommelier}"
: "${SOMMELIER_ORCH_TOKEN:=$SOMMELIER_DATA/orchestrator.token}"
: "${SOMMELIER_HUB_HOST:=0}" "${SOMMELIER_LOCAL:=http://127.0.0.1:7300}" "${SOMMELIER_UI:=$_sw_top/bus/ui}" "${SOMMELIER_TUNNEL:=}"
if [ -z "${SOMMELIER_BIN:-}" ]; then
  if [ -x "$SOMMELIER_DATA/bin/sommelier.exe" ]; then SOMMELIER_BIN=$SOMMELIER_DATA/bin/sommelier.exe; else SOMMELIER_BIN=sommelier; fi
fi
: "${SOMMELIER_ROSTER:=$SOMMELIER_DATA/roster.tsv}" "${SOMMELIER_QUOTAS:=$SOMMELIER_DATA/quotas.tsv}"
export SOMMELIER_ROSTER SOMMELIER_QUOTAS SOMMELIER_BIN
export SOMMELIER_REPOS SOMMELIER_CLONE SOMMELIER_INTEGRATE SOMMELIER_RUNS SOMMELIER_STATE
export SOMMELIER_TITLE_EXE SOMMELIER_TITLE_ARGS SOMMELIER_TITLE_DIR SOMMELIER_TITLE_PROCESS SOMMELIER_TITLE_LOG SOMMELIER_TITLE_BUILD SOMMELIER_TITLE_RUN_ARGS
export SOMMELIER_UPSTREAM SOMMELIER_FORK SOMMELIER_ORCH SOMMELIER_URL SOMMELIER_DATA SOMMELIER_HUB_HOST SOMMELIER_LOCAL SOMMELIER_UI SOMMELIER_TUNNEL

# sommelier_need NAME "what it is": stops the script with a message when a setting has no value.
sommelier_need() {
  eval "_v=\${$1:-}"
  [ -n "$_v" ] || { echo "$(basename "$0"): the setting $1 is missing ($2). See swarm/env.sh." >&2; exit 2; }
}
