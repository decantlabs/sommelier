# Swarm harness

The scripts that let one coordinator run many AI agents on this repository without them
stepping on each other. It grew on 24 Nov 2026, when up to 20 agents worked at once. It needs
Git Bash on Windows today (it uses `tasklist`, `taskkill` and PowerShell for the game window).

The rules and scheduling logic of the coordinator are in [COORDINATOR.md](COORDINATOR.md), so that any
participant can run it. The rules every worker reads are in [RULES.md](RULES.md). The raw notes of the first evening,
with what went wrong, are in [LESSONS.md](LESSONS.md). The message bus is
documented in [../bus/docs](../bus/docs/): [JOIN.md](../bus/docs/JOIN.md) for guests,
[AGENTS.md](../bus/docs/AGENTS.md) for agents, [OPERATING.md](../bus/docs/OPERATING.md) for the hub.

## Settings

All scripts source [env.sh](env.sh). Every setting comes from the environment and has a default,
except the one that names the title: `SOMMELIER_TITLE_EXE` (`run-title.sh` stops and says so
when it is missing). The list with defaults is at the top of `env.sh`. No token lives in this
folder: `SOMMELIER_ORCH_TOKEN` only names the file that holds the orchestrator's token.

## Roles

- **Coordinator** (named "main", the orchestrator account `SOMMELIER_ORCH` on the bus). Hands out work, reads READY and STUCK, starts the landing script, answers failures. Posts to GitHub.
- **Workers**. Claude, Codex and Antigravity agents, one worktree and one branch each. Codex and Antigravity are started by `codex-agent.sh` and `agy-agent.sh`.
- **Guests**. Other contributors and their agents on the bus. They see only the shared repository, get offers (TASK, RUN_REQUEST) they may ACCEPT or DECLINE, and are never ordered.

## Landing protocol

1. A worker makes its worktree with `wt.sh <folder> <branch>`, commits in small steps and pushes the branch.
2. It sends `READY <branch> <sha> : <how verified>` and carries on.
3. The coordinator runs `integrate.sh <branch> ...`. One landing at a time (lock in `SOMMELIER_STATE`). It rebases every branch on the shared main in a clean checkout, then runs the repository check, the workspace tests and clippy, and pushes to main. A red step stops the landing and prints `RESULT FAILED at <step>`.
4. `RESULT LANDED` is passed on as `LANDED`. A conflict or a failure goes back to the worker.

A red main stops all landings, so its fix goes first.

## One game run at a time

`run-title.sh <worktree> <seconds> <label> [VAR=value ...]` runs the title once. It takes a ticket in
`$SOMMELIER_RUNS/.queue`, waits for the lock folder `$SOMMELIER_RUNS/.lock`, runs, takes window screenshots
with `shot.ps1`, and collects log, pictures and a `summary.txt` in `$SOMMELIER_RUNS/<time>-<label>/`. The
owner of the machine can hold the queue with `$SOMMELIER_RUNS/.pause` or slow the title down with
`$SOMMELIER_RUNS/.gentle`. The title stays offline.

## Watchdog

`watchdog.sh` is a loop without a model. It looks every minute and ends when something needs a
decision (so whoever started it is woken). `--once` makes one pass and prints.

Repairs itself, with a line in `$SOMMELIER_RUNS/watchdog.log`: a hung screenshot step, a game lock whose
owner is gone, a title running without a lock, dead queue tickets and agent markers (the agent's bus
row goes to idle), and, when `SOMMELIER_HUB_HOST=1`, a dead hub or tunnel.

Reports, each once: a run that overstays its time, a run waiting more than 20 minutes, a landing
that is silent for 8 minutes, an external agent that finished or is silent for 20 minutes, the bus
not answering at its public address, no listener of the coordinator, and work on the bus that is
unanswered, late or declined (`bus-ledger.py`). The bus steps run only when `SOMMELIER_URL` is set.

`WATCHDOG_FOREVER=1` keeps the watchdog running and appends its findings to
`$SOMMELIER_RUNS/watchdog.findings` (a small waiter wakes the coordinator when that file grows).
Every pass it resends the orchestrator's own agent rows from `$SOMMELIER_ROSTER` and, one minute after the
time in a row of `$SOMMELIER_QUOTAS` (tab separated: who, what, gone, back, note), reports that the quota
of that participant is back. `bus-listen.sh` holds one open bus connection and collects the
messages that need the coordinator (READY, STUCK, RUN_RESULT, FIX, HELLO and the topics quota and
capacity) in `$SOMMELIER_RUNS/bus.inbox`.

## Landing queue

`lander.sh` runs forever. Append a ref to `$SOMMELIER_RUNS/landing.queue` (for example `origin/topic/name`
or a guest's `upstream/topic/name`) and it lands queued branches batch after batch. A batch is
rebased onto the current main in `$SOMMELIER_INTEGRATE` (a branch that conflicts is left out and
reported), then the repository check, the test build, the tests of the crates that need no GPU
and clippy run without the game lock, and only the tests of the GPU crates hold it. A green batch is pushed to main and announced as `LANDED`. A red batch
goes to `landing.failed`. Results are appended to `$SOMMELIER_RUNS/watchdog.findings`. The landing lock
(`$SOMMELIER_STATE/.integrate.lock`, shared with `integrate.sh`) carries its owner pid and is taken over
from a dead lander.

A branch that changes instruction files, workflows, scripts, build steps, `Cargo.lock`, `swarm`,
`bus` or `repo-check` is HELD (listed in `landing.held`) until the coordinator has read its
diff and appended `<ref> <short sha>` to `$SOMMELIER_RUNS/landing.approved`. Then queue it again.
`wake.sh` returns at once for a landing result, STUCK, RUN_RESULT and a READY that reached the inbox
unresolved, and collects the rest in batches. The `LANDED` and poll messages the scripts send are one
short line each.
`github-wait.sh` feeds the agent that keeps GitHub current.

## Quota rule

When any participant runs out of quota, its open work does not wait. The participant sends `STUCK`
on the topic `quota` with the time it is back and its open threads (branch, done, left), and the
coordinator hands them to whoever has capacity. The aim is 20 to 30 agents working at any time, so
every participant states its capacity in HELLO and keeps one status row per worker fresh. See
"Capacity and status" in [AGENTS.md](../bus/docs/AGENTS.md).

## External agents

`codex-agent.sh <name> <folder> <branch> <prompt file> [model]` and `agy-agent.sh` (same arguments)
make the worktree if needed, run the agent with the prompt, and keep its row on the bus current
(running, then idle). They write a pid marker while alive and a `.done` file with the exit code and
the READY or STUCK line at the end. The watchdog reads those. The agent gets no bus token: it
reports through its branch and its last message. Both start the agents without permission prompts,
which the project owner allowed. Do that only in a worktree you can lose.

## Budget rules from the first evening

Read LESSONS for the reasons. Short form:

- Project the burn rate after the first hour, state it in every status report, and set a ceiling per day before you start.
- An agent message wakes the coordinator with its whole context. Workers send READY or STUCK only. Progress goes to a file.
- No timers or poll loops inside agents. A script or the coordinator triggers them.
- The scarce resource sets the size of the swarm: one game, one GPU. More agents do not shorten a serial chain.
- Do not build heavily while a game run measures frame times.
- A red main blocks everyone, fix it first.
- Never edit a script in place while others run it: write a new file and `mv` it over.
- Announce a restart of a shared service before you do it.
- Decide where shared work lives (the shared repository) before inviting guests.
- Say what was seen, and name lists by what they are.
