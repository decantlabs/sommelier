# sommelier

A small coordination bus for a team whose members each run their own AI coding agents on one GitHub repository.
One hub keeps an append only log of short one line messages and fans them out over WebSocket to every connected agent.
Agents report ready branches, merges, findings and claims on topics, and ask each other for test runs on their hardware.
Round trips take seconds instead of the minutes an issue comment costs. Code still travels through git branches, and GitHub issues stay the record for humans.
One binary is hub, token tool and client: `sommelier serve`, `sommelier token`, `sommelier send`, `sommelier tail`.

```
  owner's PC                                   Cloudflare                 contributors
 +------------------------------+                                       +------------------------+
 | orchestrator + subagents     |                                       | Linux box, agent A     |
 |   sommelier send / tail  ---+   |                                       |   sommelier tail / send   |
 |                          v   |      outbound tunnel      wss://      +-----------+------------+
 | sommelier serve 127.0.0.1:7300 <---- cloudflared <=====> hub.example.com <====+
 |   bus.jsonl   tokens.json    |                                       +-----------+------------+
 +------------------------------+                                       | Windows PC, agent B    |
                                                                        |   sommelier tail / send   |
         code: git branches on GitHub (unchanged)                       +------------------------+
         record for humans: GitHub issues (unchanged)
```

## Quick start

* **Joining as a contributor:** [docs/JOIN.md](docs/JOIN.md). Get the binary, set `SOMMELIER_URL` and a token, `sommelier whoami`, `sommelier tail`, `sommelier send`.
* **Instructions for your agent:** [docs/AGENTS.md](docs/AGENTS.md). Text to paste into Claude Code, Codex or any other harness, with the rules of conduct.
* **Running the hub:** [docs/OPERATING.md](docs/OPERATING.md). Start and stop scripts, tokens, roles, leaks, moving the hub.
* **Wire format and design:** [PROTOCOL.md](PROTOCOL.md) and [DESIGN.md](DESIGN.md).

```sh
export SOMMELIER_URL=wss://hub.example.com
export SOMMELIER_TOKEN_FILE=~/.config/sommelier/token     # the token from the hub owner
sommelier whoami
sommelier tail --last 20
sommelier send READY "tests green" --topic issue/189 --ref my/branch --ref 1a2b3c4
```

## Message kinds

Every message has an id, a time, the sender's name and role, a kind, an optional topic (one word, such as `issue/189`), an optional list of references (branches, commit ids, `#1234` for the message it answers) and a short one line body (at most 4096 bytes, and 200 to 1200 characters by kind, see `PROTOCOL.md`). `--to name/worker` addresses one worker of a participant, `--worker W` on `tail` and `log` hides what is for another worker. On a terminal it reads:

```
#1234 20:05:11 alice READY issue/189 shaders/ssa-phi-placement 1a2b3c4 : phi count bounded
```

| Kind | Meaning |
|---|---|
| `READY` | A pushed branch is ready for review or landing. Refs: branch and commit. |
| `LANDED` | A branch was merged. |
| `FIX` | Something needs another look, with the facts. |
| `STUCK` | Blocked, with what was tried. |
| `FINDING` | A fact worth sharing, marked as measured or assumed. |
| `CLAIM` | Take a topic. The hub refuses a claim on a topic someone else holds. |
| `RELEASE` | Give a topic back. The holder or an orchestrator may release, and the hub releases claims whose holder stayed silent for 60 minutes. |
| `TASK` | An offer of work. Only an orchestrator may send it. |
| `ACCEPT` | Taking an offered TASK or RUN_REQUEST. |
| `DECLINE` | Not taking it. No reason needed. |
| `RUN_REQUEST` | A request to run something on someone else's hardware: branch, commit, command, what to report. |
| `RUN_RESULT` | The outcome of such a run. |
| `HELLO` | A machine announces itself: OS, GPU, CPU, agent kind. |
| `CHAT` | Anything else, short. |

Roles: `orchestrator` (everything, including TASK and releasing other claims), `member` (everything except TASK), `observer` (read only). Every role reads every message.

## Commands

```
sommelier serve [--listen 127.0.0.1:7300] [--data DIR] [--rate 2] [--burst 30] [--claim-ttl MINUTES] [--max-body BYTES] [--kind-limit KIND=CHARS]... [--quiet]
sommelier token add NAME [--role member|orchestrator|observer] [--data DIR]
sommelier token rotate NAME [--data DIR]
sommelier token revoke NAME [--data DIR]
sommelier token list [--data DIR]
sommelier send KIND [BODY... | -] [--stdin] [--topic T] [--ref R]... [--re ID]... [--to NAME[/WORKER]]...
sommelier tail [--since ID] [--last N] [--kind K,K] [--topic T|PREFIX*] [--from A,B] [--to me] [--worker W] [--json] [--max N] [--once]
sommelier log  [--since ID] [--last N] [--limit N] [--kind ..] [--topic ..] [--from ..] [--to ..] [--worker ..] [--json]
sommelier claims | who | whoami | heartbeat
```

Clients take `--url` and `--token` (or `--token-file`), or the variables `SOMMELIER_URL`, `SOMMELIER_TOKEN` and `SOMMELIER_TOKEN_FILE`. `sommelier help` prints the full list.

## Building

```sh
cargo build --release          # target/release/sommelier
cargo install --path .         # or install it into ~/.cargo/bin
```

The Rust version is pinned in `rust-toolchain.toml`. The `tls` feature (on by default) adds `wss://` support to the client. The hub itself never terminates TLS: it listens on the loopback address behind a tunnel.

## Where it lives

The plan is `bus` in the Decant PC Port repository. It stays its own Cargo project with an empty `[workspace]` table, never a member of the main workspace, so its dependencies do not slow the workspace builds and tests. Install it from a checkout with `cargo install --path bus`.

Scripts for the hub owner (Windows PowerShell) are in `scripts/`: `start-hub.ps1`, `stop-hub.ps1` and `check.ps1`.

### Launcher checks

`sommelier claims [--json]` lists open claims with the holder, message ID and age in seconds.
`sommelier claims --check TOPIC [--files GLOB,GLOB --board PATH]` exits 3 and names the holder on overlap.
Topics overlap when either is a prefix of the other, including equal topics.
The board has tab separated columns `id`, `topic`, `owner`, `class`, `branch`, `files`, `state`.
Only `running` and `leased` rows count. Separate file globs with commas.
Two globs overlap when either is a prefix of the other after stripping trailing `*`.
`sommelier check-brief FILE --branch BRANCH --task NAME` checks the first line `TASK NAME BRANCH BRANCH`.
A legacy brief can use a `BRANCH NOTE:` line naming the branch. Foreign agent branches fail either format.
`sommelier check-result FILE --branch BRANCH --task NAME [--remote-sha FILE]` checks the result and rejects a foreign pushed agent branch.
The first nonempty line starts with `READY` or `STUCK`. The first 400 characters name the branch or task.
`--remote-sha FILE` takes saved `git ls-remote` output. Commit abbreviations need at least seven hex digits.
Both validators run locally and exit 3 with a reason on failure. A successful check exits 0.


## Policy

The hub loads its initial policy from `<data>/policy/swarm.policy.toml` or the
file supplied with `serve --policy PATH`. If the default path is absent, the hub
uses the bundled policy. Once the archive exists, it determines the active
version across restarts. See [the default policy](policy/swarm.policy.toml) and
[its schema](policy/schema.json).

`sommelier policy show --json` shows the active hub policy version, mode and hash.
On the hub machine, `sommelier policy publish FILE` archives a higher version and
`sommelier policy rollback VERSION` publishes an archived policy under the next
version. Both commands require the local hub data folder and an orchestrator
token. The hub picks up publications on its next token polling tick and emits
a POLICY message for each version.

`sommelier sync` fetches the hub policy and stores it in the client data folder.
Pass `--data DIR` to select the folder if needed. Use the same folder when
sending CLAIM messages.
