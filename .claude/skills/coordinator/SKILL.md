---
name: coordinator
description: Coordinate the Decant swarm, schedule quota-aware work, verify and land branches safely, run GPU-safe game tests, keep live UI status and 45s heartbeats fresh, and upgrade the sommelier hub.
---

# Coordinator Playbook Skill

Use this skill when acting as the Decant swarm coordinator. Follow [swarm/COORDINATOR.md](../../../swarm/COORDINATOR.md) and [bus/docs/AGENTS.md](../../../bus/docs/AGENTS.md) alongside these operational rules.

## 1. Live UI & Heartbeat Hygiene

- Refresh the `coordinator` status row every 45 s (well under `suspect_sec = 120 s`) so the live dashboard never marks the coordinator suspect:
  ```sh
  sommelier status set coordinator --state running --model "<model>" --branch main "<current focus>"
  ```
- Clean finished or stale rows in `roster.tsv` and on the bus with `sommelier status clear <worker>` once a worker finishes or exits.
- Ensure worker launchers (`swarm/agy-agent.sh` and `swarm/codex-agent.sh`) run their 45 s background heartbeat loop (`say running "$TASK_DESC"` every 45 s, killed on exit) so active workers stay fresh without manual intervention.

## 2. Prior Art First and Licence Rule

- Before assigning or designing fixes, check the prior art folders listed in `$SOMMELIER_PRIORART` first and update them with `git pull --ff-only`.
- Reading prior art is permitted, and every ported pattern or algorithm must document its provenance (source repo, file, and commit/symbol) in commit messages or code comments.
- `decant-repo-check` still enforces licence header guards in Rust crates. Never copy code under a licence that does not fit the project into the repository.

## 3. Quota-Aware Scheduling

- Inspect current participant capacity and quota state with `sommelier quota show` (and `quotas.tsv` / `capacity.tsv`) before routing new tasks.
- Respect `STUCK` and `blocked` owners, as well as participants approaching or at quota limits; do not route new work to them until their reset time.
- Reassign stalled or abandoned work cleanly to participants with available capacity.
- Check for and pick up `wip/<task>` rescue branches pushed to `origin` by workers that hit quota or time limits.

## 4. Safe Branch Verification & Landing

Before approving or queuing any `READY` branch:
1. Verify linear history on `upstream/main` with **0 merges**:
   ```sh
   git rev-list --merges --count upstream/main..<ref>
   ```
2. Verify a clean merge tree against `upstream/main`:
   ```sh
   git merge-tree --write-tree upstream/main <ref>
   ```
3. Scan the diff for PII (tokens, `tokens.json`, emails, real names, local user folder paths) and GPL headers/code.
4. Record approval in `$SOMMELIER_RUNS/landing.approved` as `<ref> <8-char-sha>` and queue the ref in `$SOMMELIER_RUNS/landing.queue`.

## 5. GPU-Safe Game Testing

- Never run `swarm/run-title.sh` while `swarm/lander.sh` holds `.integrate.lock` for a code batch.
- Wait for the integration lock to clear before starting a GPU test run, then post `RUN_RESULT` on the bus (under 400 characters, with commit, seconds, frame count, last DLL, and stop/screenshot summary) and update `$SOMMELIER_RUNS/STATE.md` (or `state.md`).

## 6. Hub upgrades

When upgrading `sommelier.exe`:
1. Smoke-test the new `sommelier.exe` binary on port `7399` before touching the live `serve` daemon.
2. Only after the smoke test passes, replace the live `serve` daemon binary.
3. Restart `bus-listen.sh` so the coordinator listener reconnects cleanly.
