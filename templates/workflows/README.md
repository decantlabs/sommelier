# Workflow templates of the factory

Three GitHub Actions workflows for a project that runs the sommelier factory (`docs/factory.md`, sections 8.3,
8.5, 9.2 and 10). They are copied into the project's repository under `.github/workflows/`. `.github/**` must
be a hold path there, so a patch can never change what checks it.

None of them uses a secret, the pull request target trigger, a cache action, or a script taken from a patch.
Never add a cache action to any of these workflows: a cache is shared state, and code of a patch could write an
entry that the batch or release workflow restores later. Lines and values
marked `PROJECT` are placeholders that the project fills in.

## factory-verify.yml

The clean build of one submission (S2). The hub starts it with `workflow_dispatch` and reads only the
conclusion of the run through the GitHub API with a read only token. The job has `permissions: {}`.

Inputs, all required:

| Input | Meaning |
|-------|---------|
| `submission` | the SHA 256 of the bundle, 64 lower case hex characters |
| `order` | the order id |
| `base` | the base commit the bundle applies on |
| `hub` | the base URL of the hub, https only |
| `attempt` | the attempt number of the submission |
| `dispatch` | how often the hub dispatched this attempt |

The run name is `factory-verify <submission> a<attempt> d<dispatch>`, which is how the hub finds its run. The
concurrency group is `factory-verify-<submission>` and never cancels a running job. The time limit is 30 minutes.
The steps are named and stable, because the hub reports the name of the failing step: Check inputs, Check out
base, Fetch bundle, Check bundle hash, Import bundle ref, Fetch dependencies, Read done condition, Write the
container script, Build, test and run done condition, Check the done condition again.

The job downloads the bundle by hash, checks the hash, fetches the ref `refs/sommelier/submit` of the bundle
with hooks off, and fetches the dependencies of the lock file with `cargo fetch` (this step runs no code of the
patch). Then ONE step builds, tests and runs the `done_when` commands in a fresh container: no network, no
sudo, all capabilities dropped, a read only root, a clean environment (`env -i`, a fixed `PATH`,
`bash --noprofile --norc`). The script of the container is written by the workflow, not taken from the checkout.
Nothing that the patch can write is read by a later step, and the runner files that steer later steps
(`GITHUB_ENV`, `GITHUB_PATH`) are not mounted. The done condition is read from the JSON of the hub with `jq`
before the build and fetched again after it. A different second copy fails the run.

What a green run proves: the patch builds, passes the tests and passes the done condition of the order, and
the hub gave the same done condition before and after. It proves that for an honest patch. It does NOT prove
anything against a hostile patch: build code and test code of the patch run in the job and can try to fool it.
The container makes that hard, not impossible. The assigned review is the stage that stops a hostile patch.

What a project fills in:

* the pinned commit of each `uses:` action and the digest of the build image,
* the hub paths of the bundle (`/api/bundle/<sha256>`) and of the done condition (`/api/order/<id>/done_when`),
  which the hub wave defines,
* the dependency fetch, the selection of the touched crates, the build and the test commands.

## factory-batch.yml

The second lock (S4). It runs on a push to `landing/**` and its job is named `factory-batch`. It has
`contents: read` only.

It runs the full workspace checks once per batch. A guarded entry needs a maintainer signature that is checked
against the keys listed on the default branch. That check is not built yet, so the step fails closed: it fails
when a commit of the batch carries the trailer `Factory-Guarded:` or when a changed path matches `GUARDED_PATHS`.
The path rule is there because a taken over hub could leave the trailer out. `GUARDED_PATHS` must stay equal to
the hold list of the hub (`bus/src/gate/rules.rs`) and to `GUARD` in `swarm/lander-lib.sh`.

What a project fills in: `GUARDED_PATHS` (add paths, never remove one), the pinned commit of the checkout
action, and the three check commands.

### Setting up the second lock (the owner does this by hand)

The file above is NOT a lock by itself. A push to `landing/**` makes GitHub take the workflow file from the
pushed commit, so a patch that edits `factory-batch.yml`, or adds any job named `factory-batch`, removes the
fail closed step and the check turns green. The check must come from a workflow that the batch cannot change.
The owner of the repository sets this up in the settings. No script can do it, and the factory never does it:

1. Keep `factory-batch.yml` on the default branch.
2. In the repository settings, add a ruleset on the default branch (Settings, Rules, Rulesets). Turn on
   "Require workflows to pass before merging" and pick `.github/workflows/factory-batch.yml` from the
   default branch. A required workflow runs from the protected branch, so the copy inside a batch is not what
   decides. Also require the status check `factory-batch` from the GitHub Actions app, and turn on
   "Block force pushes" and "Restrict deletions" for the default branch.
3. Make `.github/**` a hold path of the hub, and keep it in `GUARD` of the lander. A batch whose diff touches
   `.github/` is refused unless a maintainer approved that commit.
4. Tell the lander which workflow and which app made the check: `SOMMELIER_BATCH_WORKFLOW` (default
   `.github/workflows/factory-batch.yml`) and `SOMMELIER_BATCH_APP` (default `github-actions`). The lander
   counts a check only when it has the right name, the right app and belongs to a run of that workflow file.

Check the setup once by hand: push a branch `landing/test` that edits the workflow to always succeed, and see
that main still refuses the merge because the required workflow of the default branch failed or did not run.

## release.yml

On a push to the default branch: builds the release binaries, creates a provenance attestation and attaches
both to a release named `main-<12 characters of the commit>`. Testers run only builds with this attestation. The
build job has `contents: read`. Only the publish job has write permissions, and it uses the token GitHub gives
the run. The `candidate` channel of section 9.2 needs a second trigger, which a later wave adds.

What a project fills in: the build command, the binary names, the pinned commits of the actions.

## Points to check against the live documentation before first use

These were written from memory of long stable features, without network access.

1. The dispatch endpoint `POST /repos/{owner}/{repo}/actions/workflows/{file}/dispatches` answers 204 without a
   run id. Check whether the current API can return the run id, which would replace the search by run name.
2. The REST version header `X-GitHub-Api-Version: 2022-11-28` is still accepted.
3. The list of runs with `event=workflow_dispatch`: the field that holds the `run-name`. GitHub puts the name
   of the workflow in `name` and the text of `run-name:` in `display_title`, so the hub must match
   `display_title` first (this must be checked against the live API before first use). Also that a run appears
   within the wait of three minutes.
4. The jobs endpoint with `filter=latest` and the fields `steps[].conclusion`, `steps[].number`.
5. That a job that hits `timeout-minutes` reports the conclusion `failure` or `timed_out` and which step is marked.
   The parser reads a `timed_out` job or step as timed out.
6. That `permissions: {}` leaves `actions/checkout` able to read a public repository. If not, `contents: read`
   is the smallest block that works.
7. That a token with `actions: write` and `contents: read` on the repository is the least that may dispatch, and
   that `actions: read` is enough for the run and job reads. A fine grained token is expected.
8. The limit of inputs of `workflow_dispatch` (the number of inputs and the size of a value).
9. That `docker run --network none --read-only --cap-drop ALL --user <runner uid>` works on the `ubuntu-24.04`
   hosted image with the workspace and `RUNNER_TEMP` mounted, that `jq` is installed there, and that the
   runner user can write the mounted workspace from inside the container.
10. The current major version and the commit SHA of `actions/checkout`, `actions/upload-artifact`,
    `actions/download-artifact` and `actions/attest-build-provenance`, and the permissions the attestation
    action needs (`id-token: write`, `attestations: write`, `contents: read`).
11. That the ruleset can require the check name `factory-batch` and that the job `name:` is the check name.
12. That `github.event.repository.default_branch` is set for a push event.
13. Free plan limits: 20 concurrent jobs, and the rate limits for dispatch and for reading runs.
14. That an attestation can be verified by the client with `gh attestation verify` offline or against the
    API, as the tester client needs.
15. That a ruleset can require a workflow from the default branch ("Require workflows to pass before merging")
    on the plan of the project, that the run of such a workflow carries the path `.github/workflows/factory-batch.yml`
    in the field `path` of the workflow run (the lander matches with `contains`), that the check run has
    `app.slug` equal to `github-actions` and `check_suite.id` equal to the `check_suite_id` of that run.
16. That `GET /repos/{owner}/{repo}/actions/runs?head_sha=<sha>` lists the runs of a pushed `landing/**` commit.
17. That the base image named in `VERIFY_IMAGE` (a Rust toolchain with git) exists, and its digest.
18. That `cargo fetch --locked` in a container with the checkout mounted read only still works for git
    dependencies, and that the copy of its registry into a tmpfs cargo home is complete for `--offline`.
