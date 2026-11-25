# Operating the public factory

For the owner of a hub. The design is `docs/factory.md`. The hub itself is `bus/docs/OPERATING.md`. Steps marked
**owner's hands** cannot be done by an agent or by the hub: they are GitHub settings or a rented server. Where a
command does not exist yet, this page says what is planned and invents no flags.

Status today: every part of the factory is wired into the hub and tested together (`bus/tests/e2e.rs`). Nothing of it
is on by default. A hub is switched on with `sommelier serve --factory FILE`, a local TOML file (see
`swarm/factory.example.toml`, one table per part, every key explained). The numbers that are policy (tier rules,
review rule, audit share, verify limits, quorum, proof of work bits) come from the published policy when the hub
starts, and a policy switch that says `off` vetoes a part the file enables. At start the hub prints one line,
`factory: join on, intake on, verify off, ...`. To watch the whole way once, run `sh swarm/factory-demo.sh PORT`: it
builds the binary, starts a throwaway hub with no GitHub (the owner marks the clean build with `sommelier verify
override`), and walks one patch from the join of a stranger to its landing, then a test order with three reports.
`sommelier factory status --data DIR` prints the submissions, reviews, landing queue, tiers and run orders of a hub
data folder, and `sommelier op OP JSON` sends one module request.

## 1. Before the repositories go public (phase F0)

All of these must be true. Each has a check.

1. **Pull requests limited to collaborators.** Owner's hands: repository settings, pull request access. Check:
   an account with no rights cannot open a pull request.
2. **A ruleset on the default branch** that requires the workflow `factory-batch` as a REQUIRED WORKFLOW taken
   from the protected default branch, and lets nobody push past it. Owner's hands, no script does it. A check
   that the batch itself can redefine is no lock: a push to `landing/**` runs the workflow file of the pushed
   commit, so a patch could edit it or add any job named `factory-batch`. The steps are in
   `templates/workflows/README.md`, "Setting up the second lock". Check: a direct push to main is refused, and a
   `landing/test` branch whose workflow file always succeeds still cannot be merged. This is the second lock of
   `docs/factory.md` 8.5.
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

With `--factory` the proof of work comes from the policy `[join]` table, and `--join-pow-bits` still wins when given.
The limits per address block and the public read of the bus are the `[door]` table of the factory file.

## 3. Where the secrets live

* **Hub data folder** (`--data`, else `SOMMELIER_DATA`, else the default data folder). It holds the log `bus.jsonl`,
  the token file `tokens.json` (salted hashes only), the hub id, `challenge_secret` and, when `keep_days` is above 0,
  `door.db`. It is as sensitive as the addresses of the members: with `challenge_secret` and `door.db` anyone
  can recover the address blocks (see the front door above, and `secret_file`). Keep it private and back it up with
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

### The front door

Settings are the `[door]` table of the factory file (`swarm/factory.example.toml` ships limits that are safe when
copied as they are, and a public address to change).

* **`public_addresses` is required for key sign in.** A key signs the address it connected to and the hub takes it
  only for these names and for loopback. List the host name that members type, for example `hub.example.org`
  (ports 443 and 80) or `hub.example.org:7300`. With an empty list members can sign in at loopback addresses only,
  and the refusal says so. This is what stops a hostile hub from relaying a member's sign in to yours.
* **The data folder is as sensitive as the addresses.** The address blocks are an HMAC of the /24 of an IPv4 address,
  and there are only 16 million of those: whoever has the key inverts every block in seconds. By default the key is
  derived from `challenge_secret` in the data folder, which sits beside `door.db`. So a copy or a backup of the data
  folder, together with `door.db`, reveals where members connected from. Either keep `keep_days = 0`, or set
  `secret_file` to a file outside the data folder and outside its backups.
* **Behind a tunnel.** `trust_header` names the header that carries the client address, and the hub honours it only
  for a loopback peer. Behind Cloudflare, `CF-Connecting-IP` is overwritten by the edge and is safe. A header that
  a tunnel or a proxy passes through unchanged lets a client choose its own block. Use a header that your proxy
  sets itself, and check that a request with that header made by hand is overwritten.
* **Connection limits.** Set `conns_per_member` and `max_conns` (the example ships 4 and 400), the plain HTTP
  limits `max_http_conns` and `http_conns_per_block`, and `max_upload_bytes_in_flight`. With `max_conns = 0` the
  hub holds as many connections as the machine can. Every token, observer tokens included, counts as a member
  above tier 0, so the dashboard and the witness are not pushed out by anonymous readers.
* **Public read.** The public view is an allow list (PROTOCOL.md section 14). A key member below tier 2 gets the
  same view. Read it before you switch `public_read` on.

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
* [ ] 9. `docker run --network none --read-only --cap-drop ALL` works on the `ubuntu-24.04` hosted image with the
  workspace mounted, and `jq` is installed there.
* [ ] 10. The current major version and commit SHA of `actions/checkout`, `actions/upload-artifact`,
  `actions/download-artifact` and `actions/attest-build-provenance`, and the permissions the attestation needs.
* [ ] 11. The ruleset can require the workflow `factory-batch` from the default branch, and the job `name:` is
  the check name. The check run has the app `github-actions` and the path of the workflow in its run.
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
  `sommelier policy rollback VERSION`. The factory switches and the scheduler's keys (`scheduler`, `admission`,
  `probe_<vendor>`) live in the one `[switch]` table of one policy file. A switch that says `off` vetoes the part even
  when the local factory file enables it. The hub reads the policy numbers when it starts.
* **Revoke a token:** `sommelier token revoke NAME`. A running hub picks it up within seconds.
* **Disable a workflow** in the repository settings. Owner's hands. Disabling `factory-batch` makes landing stop
  at the second lock, which is the safe direction.
* **Stop the hub.** The factory stays closed while it is down. The standby (`sommelier standby`, `sommelier
  promote`) follows the log and takes over only with a confirmation code.
