#!/bin/sh
# Walks one patch of a stranger through the whole factory with the real binary, for a person to watch.
#
#   sh swarm/factory-demo.sh PORT        PORT is a free port on this machine, never 7300
#
# It builds sommelier, makes a temp folder with a hub data folder, a project repository and two client
# data folders (a stranger and a reviewer), starts a hub on PORT with every part of the factory on, and
# runs the way from the join of the stranger to the landing of its patch. After each step it prints the
# SUBMISSION states and the landing queue. At the end it runs a test order that three testers answer with
# `report.send`. The hub stops and the temp folder goes away at the end, also when a step fails.
#
# The hub runs without GitHub: the clean build is marked passed by the owner with `verify override`, the
# way an owner goes on while GitHub is down. Nothing here touches a live hub or the network.
set -eu

usage() {
  echo "usage: sh swarm/factory-demo.sh PORT   (a free port, not 7300)" >&2
  exit 2
}
[ $# -ge 1 ] || usage
PORT=$1
case $PORT in '' | *[!0-9]*) usage ;; esac
if [ "$PORT" = 7300 ]; then
  echo "factory-demo: port 7300 is where a live hub runs, pick another port" >&2
  exit 2
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d)
HUB_PID=""

cleanup() {
  code=$?
  if [ -n "$HUB_PID" ]; then
    kill "$HUB_PID" 2>/dev/null || true
    wait "$HUB_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
  if [ "$code" -ne 0 ]; then
    echo "factory-demo: stopped with exit code $code, the hub is stopped and the temp folder is removed" >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

step() { printf '\n== %s\n' "$*"; }

step "build sommelier"
(cd "$ROOT/bus" && cargo build --quiet)
BIN=${CARGO_TARGET_DIR:-$ROOT/bus/target}/debug/sommelier
[ -x "$BIN" ] || BIN=$BIN.exe

HUB=$TMP/hub
S=$TMP/stranger
R=$TMP/reviewer
PROJECT=$TMP/project
MIRROR=$TMP/mirror.git
WORK=$TMP/work
URL=ws://127.0.0.1:$PORT
mkdir -p "$HUB" "$S" "$R"

g() { git -c user.name=Demo -c user.email=demo@example.org -c commit.gpgsign=false -c core.autocrlf=false "$@"; }

# Who runs a command: the owner and the testers with a token, the two members with their key.
owner() { SOMMELIER_URL=$URL SOMMELIER_TOKEN=$OWNER_TOKEN "$BIN" "$@"; }
stranger() { (unset SOMMELIER_TOKEN SOMMELIER_TOKEN_FILE; SOMMELIER_URL=$URL SOMMELIER_DATA=$S "$BIN" "$@"); }
reviewer() { (unset SOMMELIER_TOKEN SOMMELIER_TOKEN_FILE; SOMMELIER_URL=$URL SOMMELIER_DATA=$R "$BIN" "$@"); }

show() { "$BIN" factory status --data "$HUB"; }

# A path as the hub binary reads it: the Windows form under Git Bash, the same path elsewhere.
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# wait_for WHAT PATTERN: waits until the status lists PATTERN, at most a minute. A failed state ends the demo.
wait_for() {
  n=0
  until show | grep -q "$2"; do
    if show | grep -q "state [a-z_]*_failed"; then
      echo "factory-demo: a submission failed while waiting for $1" >&2
      show >&2
      exit 1
    fi
    n=$((n + 1))
    if [ "$n" -gt 60 ]; then
      echo "factory-demo: timed out waiting for $1" >&2
      show >&2
      exit 1
    fi
    sleep 1
  done
}

step "the project repository and its mirror (the reference of the hub)"
mkdir -p "$PROJECT/src"
(
  cd "$PROJECT"
  g init -q -b main
  printf 'pub fn one() -> u32 {\n    1\n}\n' >src/lib.rs
  g add -A
  g commit -q -m base
)
BASE=$(g -C "$PROJECT" rev-parse HEAD)
g clone -q --bare "$PROJECT" "$MIRROR"
g clone -q "$PROJECT" "$WORK"
echo "base commit $BASE"

step "settings of the hub: a policy with rule numbers lowered for the demo, and the local factory file"
cp "$ROOT/bus/policy/swarm.policy.toml" "$TMP/policy.toml"
cat >>"$TMP/policy.toml" <<'EOF'

[trust.tier1]
reach = { landed = 5, run_agreed = 1, key_age_days = 0, reverted_for_cause = 0 }

[trust.tier2]
reach = { landed = 0, review_confirmed = 0, key_age_days = 0, reverted_for_cause = 0 }

[test]
quorum = 3
candidate_min_tier = 2

[switch]
intake = "enforce"
tier0_coding = "enforce"
EOF
# The verify part stays off: there is no GitHub here, the owner overrides it below.
cat >"$TMP/factory.toml" <<EOF
[join]
enabled = true
[intake]
enabled = true
reference_repo = "$(native "$MIRROR")"
[review]
enabled = true
[stamp]
enabled = true
tier0_coding = true
[test]
enabled = true
EOF
OWNER_TOKEN=$("$BIN" token add owner --role orchestrator --data "$HUB" 2>/dev/null)

step "start the hub on $URL"
"$BIN" serve --listen "127.0.0.1:$PORT" --data "$HUB" --policy "$TMP/policy.toml" \
  --factory "$TMP/factory.toml" --join-pow-bits 10 --no-ui --quiet 2>"$TMP/hub.log" &
HUB_PID=$!
n=0
until owner whoami >/dev/null 2>&1; do
  n=$((n + 1))
  if [ "$n" -gt 60 ]; then
    echo "factory-demo: the hub did not come up" >&2
    cat "$TMP/hub.log" >&2
    exit 1
  fi
  sleep 1
done
grep '^factory:' "$TMP/hub.log"

step "the owner adds an order with low risk, small size and min_trust 0"
printf 'Add a function two() that returns 2.\n' >"$TMP/brief.md"
owner task add --id t1 --kind fix --title "Add two" --scope src/lib.rs --done-when "cargo test" \
  --brief "$TMP/brief.md" --risk low --size small --min-trust 0 --base "$BASE"

step "a stranger joins with a key and takes the order (tier 0)"
stranger join --handle stranger | sed "s|^key file .* (|key file in the client folder (|"
stranger next
stranger work t1 --brief-out "$TMP/brief-t1.md" | sed "s| brief .*| brief saved|"

step "the stranger commits a change inside the scope and submits it"
(
  cd "$WORK"
  g checkout -q -b work/t1 "$BASE"
  printf 'pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n' >src/lib.rs
  g commit -q -am "add two"
)
SUBMITTED=$(stranger submit t1 --repo "$WORK" --base "$BASE" --attempt 1 --json)
SHA=$(printf '%s' "$SUBMITTED" | sed -n 's/.*"submission":"\([0-9a-f]\{64\}\)".*/\1/p')
[ -n "$SHA" ] || { echo "factory-demo: no submission hash in: $SUBMITTED" >&2; exit 1; }
echo "submission $SHA"
show

step "the static gates pass: the submission is gated"
wait_for "the gates" "order t1 .* state gated"
show

step "the owner marks the clean build passed (no GitHub in this demo)"
owner verify override "$SHA" --conclusion passed --reason "demo: there is no GitHub here"
show

step "a second member joins, the owner grants it tier 2, and the hub assigns it the review"
reviewer join --handle reviewer | sed "s|^key file .* (|key file in the client folder (|"
R_ID=$(reviewer key show --json | sed -n 's/.*"member":"\([^"]*\)".*/\1/p')
owner standing grant "$R_ID"
wait_for "tier 2 of the reviewer" "tier 2"
reviewer send CHAT "reviewer is here"
wait_for "the review" "order t1 .* state reviewing"
show
# The hub picks the reviewer by a keyed hash among the online members of tier 2 and above that are not the
# author. That can be the new reviewer or the owner (tier 3), so the demo asks the hub who it picked.
PICKED=$(show | sed -n 's/^  t1\/1 for [0-9a-f]* reviewer \([^ ]*\) assigned.*/\1/p')
case $R_ID in
  "$PICKED"*) WHO=reviewer ;;
  *) WHO=owner ;;
esac
as_picked() { if [ "$WHO" = reviewer ]; then reviewer "$@"; else owner "$@"; fi; }
echo "the hub picked the $WHO"
as_picked review next

step "the picked member reads the order and the diff and passes it"
printf '{"verdict":"pass","findings":[],"summary":"adds two(), nothing else"}\n' >"$TMP/verdict.json"
as_picked review verdict t1 1 --file "$TMP/verdict.json"
wait_for "the approval" "order t1 .* state approved"
show

step "the lander takes the entry and reports it landed"
owner op land.next '{"claim":true,"batch_id":"demo1"}' >/dev/null
owner op land.result '{"batch_id":"demo1","result":"landed"}' >/dev/null
wait_for "the landing" "order t1 .* state landed"
wait_for "the landed stamp" "landed=1"
show

step "the test network: a run order, three testers on THIS machine answer with report.send"
ORDER=$(owner op test.order "{\"build\":\"$BASE\",\"channel\":\"main\",\"profile\":\"demo\",\"scenario\":\"boot\",\"wanted\":3}")
RUN=$(printf '%s' "$ORDER" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
echo "run order $RUN"
NOW=$(date +%s)
MACHINE='{"os":"windows","os_build":"26100","gpu":"RTX 4080","gpu_memory_mb":16384,"driver":"552.22","memory_mb":32768}'
# The testers get their tokens only now, so they are no candidates for the review above.
for name in alice bob dave; do
  "$BIN" token add "$name" --role member --data "$HUB" 2>/dev/null >"$TMP/$name.token"
done
sleep 3 # the hub reads new tokens every two seconds
for name in alice bob dave; do
  as_tester() { SOMMELIER_URL=$URL SOMMELIER_TOKEN=$(cat "$TMP/$name.token") "$BIN" "$@"; }
  # A report counts only for an order the hub handed to this tester for this machine.
  as_tester op test.next "{\"machine\":$MACHINE,\"profiles\":[\"demo\"]}" >/dev/null
  REPORT="{\"report\":{\"v\":1,\"run\":\"$RUN\",\"member\":\"$name\",\"build\":\"$BASE\",\"channel\":\"main\",\"profile\":\"demo\",\"scenario\":\"boot\",\"machine\":$MACHINE,\"outcome\":\"works\",\"first_error\":null,\"metrics\":{},\"secs_alive\":60,\"log_excerpt\":\"1 swapchain created\n\",\"at\":$NOW}}"
  as_tester op report.send "$REPORT" >/dev/null
  echo "$name reported works"
done
sleep 3
show
echo
echo "Three testers on one machine are one owner: the front door keeps no address blocks in this demo, so"
echo "every report is a hint, the order does not settle and nobody gets run_agreed. On a real hub with the"
echo "door on, three testers in three different address blocks settle it. This is the guard against one person"
echo "with many keys."
if "$BIN" factory status --data "$HUB" | grep -q "$RUN settled"; then
  echo "factory-demo: the order settled from one machine, that must not happen" >&2
  exit 1
fi

step "the end: what the hub holds"
show
echo
echo "factory-demo: done"
