# Operating the public factory

For the owner of a hub. The design is `docs/factory.md`. The hub itself is `bus/docs/OPERATING.md`. Steps marked
**owner's hands** cannot be done by an agent or by the hub: they are GitHub settings or a rented server. Where a
command does not exist yet, this page says what is planned and invents no flags.

Status today: the key modules, gates, bundles, stamps, review choice, reports and the clean build state machine are
built as libraries. Joining with a key works. Submitting, reviewing and testing are being built. Nothing of the
factory is on by default.

## 1. Before the repositories go public (phase F0)

All of these must be true. Each has a check.

1. **Pull requests limited to collaborators.** Owner's hands: repository settings, pull request access. Check:
   an account with no rights cannot open a pull request.
2. **A ruleset on the default branch** that requires the check `factory-batch` and lets nobody push past it.
   Owner's hands. Check: a direct push to main is refused. This is the second lock of `docs/factory.md` 8.5.
3. **Private vulnerability reporting switched on.** Owner's hands. `CONTRIBUTING.md` sends security reports there.
4. **The scrub.** No old name, no real hostname and no path with a user folder in any file or in the history that
   goes public. Done when a search for those strings is empty. A scrub script does not exist yet, so the check is
   a search you run yourself. Also confirm that no token, key file or hub secret was ever committed.
5. **`.github/**` is a hold path** in the policy, so a patch can never change what checks it.
6. **The documents are in place:** `CONTRIBUTING.md`, `bus/docs/JOIN.md` and the folder table in `README.md`.

## 2. Switching joining on

Joining is off by default. A hub that is upgraded behaves as before.

1. Put the hub behind a tunnel or another front door. The hub listens on loopback only and never terminates TLS.
2. Restart it with `sommelier serve --join on --join-pow-bits N`. The default is 20 bits, at most 40. Pick a value
   that costs a laptop a few seconds.
3. From another machine run `sommelier join --url wss://hub.example.com`, then `sommelier whoami`. It must show
   tier 0.
4. To stop joins, restart with `--join off`. Be aware that `--join off` also refuses keys that joined earlier, so
   it is a lock for everybody with a key, not only for new ones.

Reading these settings from the policy file is a later step. The limits per address block and the public read of
the bus are being built.

## 3. Where the secrets live

* **Hub data folder** (`--data`, else `SOMMELIER_DATA`, else the default data folder). It holds the log `bus.jsonl`,
  the token file `tokens.json` (salted hashes only) and the hub id. Keep it private and back it up with
  `sommelier snapshot`.
* **The key book.** There is no separate file. A key is a FACT from the hub with topic `key/<member>` in the log,
  and replay rebuilds the book. Backing up the log backs up every member's public key. Private keys are only on the
  members' machines.
* **The hub secret** keys the reviewer choice and the audit sample. The code takes it as an input. A file for it in
  the hub data folder is planned and is not built. It must never be written to the log or served.
* **The push key** is on the lander machines only, never on the hub server.
* **Role seat tokens** are issued with `sommelier token add` on the hub machine and handed over privately.
* Network addresses of members are personal data and may not reach the log. They go to a side store that is
  planned.

## 4. When a key must be revoked

* A member revokes their own key with `sommelier key revoke --yes`.
* The owner revokes another key with the request `key.revoke`, which needs an orchestrator token and a reason.
  There is no command for it yet. A command is planned. Until then use the member's own path or `--join off`.
* After a revoke, the planned follow up is an audit order for every commit that key landed (`docs/factory.md` 4.4).
  Do it by hand until it is built: list the landed commits of the member and review them.
* A stolen maintainer key is removed by a guarded change on main by another maintainer, then its approvals are
  audited.

## 5. Checklist before the first verify run

The workflow templates in `templates/workflows/` were written from memory without network access and never run.
Check each point against the live GitHub documentation first. Mark it when done.

* [ ] 1. The dispatch endpoint answers 204 without a run id. Check whether the API can now return the run id.
* [ ] 2. The version header `X-GitHub-Api-Version: 2022-11-28` is still accepted.
* [ ] 3. The list of runs with `event=workflow_dispatch`: which field holds the `run-name`, and that a run appears
  within the wait of three minutes.
* [ ] 4. The jobs endpoint with `filter=latest` and the fields `steps[].conclusion` and `steps[].number`.
* [ ] 5. A job that hits `timeout-minutes` reports `failure` or `timed_out`, and which step is marked.
* [ ] 6. `permissions: {}` still lets `actions/checkout` read a public repository. If not, `contents: read`.
* [ ] 7. A token with `actions: write` and `contents: read` is the least that may dispatch, and `actions: read` is
  enough for reading runs and jobs. Use a fine grained token.
* [ ] 8. The limits of `workflow_dispatch` inputs, their number and the size of a value.
* [ ] 9. `sudo -n unshare --net` and `ip link set lo up` work on the `ubuntu-24.04` hosted image.
* [ ] 10. The current major version and commit SHA of `actions/checkout`, `actions/upload-artifact`,
  `actions/download-artifact` and `actions/attest-build-provenance`, and the permissions the attestation needs.
* [ ] 11. The ruleset can require the check name `factory-batch`, and the job `name:` is the check name.
* [ ] 12. `github.event.repository.default_branch` is set for a push event.
* [ ] 13. The Free plan limits: 20 concurrent jobs, and the rate limits for dispatch and for reading runs.
* [ ] 14. An attestation can be verified by the client with `gh attestation verify`, offline or against the API.

Also fill the `PROJECT` placeholders in each template. The `candidate` channel needs a second trigger in the
release workflow that is not built. `factory-batch` fails closed for guarded entries until the maintainer
signature check exists, so guarded changes cannot land through it yet.

## 6. Kill switches

From the narrowest to the widest.

* **Pause a pool or scope:** `sommelier pause <SCOPE>` and `sommelier resume <SCOPE>`. On a runner machine the
  file `$SOMMELIER_RUNS/.pause` stops new title runs.
* **Stop new joins:** `sommelier serve --join off` (see section 2 for what it also does).
* **Policy switches:** the `[switch]` table has `intake`, `tier0_coding` and `candidate_builds`, each `off`,
  `shadow` or `enforce`, and all are `off` by default. With `intake = "off"` nothing new comes in and members keep
  what they hold. Publish a changed policy with `sommelier policy publish FILE` and go back with
  `sommelier policy rollback VERSION`. The factory `[switch]` table and the scheduler's `[switch]` keys cannot be in
  one file yet, because two loaders read them.
* **Revoke a token:** `sommelier token revoke NAME`. A running hub picks it up within seconds.
* **Disable a workflow** in the repository settings. Owner's hands. Disabling `factory-batch` makes landing stop
  at the second lock, which is the safe direction.
* **Stop the hub.** The factory stays closed while it is down. The standby (`sommelier standby`, `sommelier
  promote`) follows the log and takes over only with a confirmation code.
