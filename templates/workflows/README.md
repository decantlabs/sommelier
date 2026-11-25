# Workflow templates of the factory

Three GitHub Actions workflows for a project that runs the sommelier factory (`docs/factory.md`, sections 8.3,
8.5, 9.2 and 10). They are copied into the project's repository under `.github/workflows/`. `.github/**` must
be a hold path there, so a patch can never change what checks it.

None of them uses a secret, the pull request target trigger, or a script taken from a patch. Lines and values
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

The run name is `verify <submission>`, which is how the hub finds its run. The concurrency group is
`factory-verify-<submission>` and never cancels a running job. The time limit is 20 minutes. The steps are named
and stable, because the hub reports the name of the failing step: Check inputs, Check out base, Fetch bundle,
Check bundle hash, Import bundle ref, Fetch dependencies, Read done condition, Build and test touched crates,
Run done condition.

The job downloads the bundle by hash, checks the hash, fetches the bundle's ref with hooks off, fetches
dependencies from the lock file, then builds and tests with the network cut where the runner allows it and runs
the `done_when` commands of the order, which it reads from the hub by order id.

What a project fills in:

* the pinned commit of each `uses:` action,
* the hub paths of the bundle (`/api/bundle/<sha256>`) and of the done condition (`/api/order/<id>/done_when`),
  which the hub wave defines,
* the dependency fetch, the selection of the touched crates, the build and the test commands.

## factory-batch.yml

The second lock (S4). It runs on a push to `landing/**` and its job is named `factory-batch`. The ruleset on the
default branch requires that check on the exact commit. It has `contents: read` only.

It runs the full workspace checks once per batch. A guarded entry needs a maintainer signature that is checked
against the keys listed on the default branch. That check is not built yet, so the step fails closed: it fails
when a commit of the batch carries the trailer `Factory-Guarded:` or when a changed path matches `GUARDED_PATHS`.
The path rule is there because a taken over hub could leave the trailer out.

What a project fills in: `GUARDED_PATHS`, the pinned commit of the checkout action, and the three check commands.

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
3. The list of runs with `event=workflow_dispatch`: the field that holds the `run-name` (`name` and
   `display_title`), and that a run appears within the wait of three minutes.
4. The jobs endpoint with `filter=latest` and the fields `steps[].conclusion`, `steps[].number`.
5. That a job that hits `timeout-minutes` reports the conclusion `failure` or `timed_out` and which step is marked.
   The parser reads a `timed_out` job or step as timed out.
6. That `permissions: {}` leaves `actions/checkout` able to read a public repository. If not, `contents: read`
   is the smallest block that works.
7. That a token with `actions: write` and `contents: read` on the repository is the least that may dispatch, and
   that `actions: read` is enough for the run and job reads. A fine grained token is expected.
8. The limit of inputs of `workflow_dispatch` (the number of inputs and the size of a value).
9. That `sudo -n unshare --net` and `ip link set lo up` work on the `ubuntu-24.04` hosted image, and which
   runner images exist then. The template continues with `CARGO_NET_OFFLINE` when they do not.
10. The current major version and the commit SHA of `actions/checkout`, `actions/upload-artifact`,
    `actions/download-artifact` and `actions/attest-build-provenance`, and the permissions the attestation
    action needs (`id-token: write`, `attestations: write`, `contents: read`).
11. That the ruleset can require the check name `factory-batch` and that the job `name:` is the check name.
12. That `github.event.repository.default_branch` is set for a push event.
13. Free plan limits: 20 concurrent jobs, and the rate limits for dispatch and for reading runs.
14. That an attestation can be verified by the client with `gh attestation verify` offline or against the
    API, as the tester client needs.
