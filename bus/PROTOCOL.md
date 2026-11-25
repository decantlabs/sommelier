# sommelier wire protocol, version 1.2

Version 1.1 adds one optional field, `to` (addressees). Frames without it are
exactly version 1, so version 1 clients keep working unchanged. Version 1.2 adds
the kind STATUS (agent rows), the request `roster`, and plain HTTP on the same
port for the dashboard (section 8). Nothing of version 1 changed.

This is everything a client needs. A client in any language that can open a
WebSocket and read and write text frames can take part without the `sommelier`
binary.

## 1. Connection

```
ws://127.0.0.1:7300/?<query>        on the hub machine
wss://hub.example.com/?<query>   through a tunnel (TLS ends at the tunnel)
```

The path is ignored. Authentication happens in the HTTP upgrade request:

* `Authorization: Bearer <token>`: the normal way, works for every role.
* `?token=<token>` in the query: accepted **for observer tokens only**, for
  clients that cannot set headers. A member or orchestrator token in the URL is
  refused with HTTP 401, because URLs end up in proxy and tunnel logs.

A missing, unknown or revoked token gets HTTP 401 with a one line text body
and no upgrade. A malformed query gets HTTP 400.

Query parameters (all optional, values percent encoded):

| name     | meaning |
|----------|---------|
| `token`  | observer token, see above |
| `since`  | replay stored messages with `id >= since` before the live stream |
| `last`   | replay the last N stored messages that match the filters |
| `live`   | `0` means no live stream: the connection only answers requests |
| `kinds`  | comma separated kinds, for example `READY,FIX` |
| `topic`  | exact topic, or a prefix ending in `*`, for example `issue/*` |
| `from`   | comma separated sender names |
| `to`     | a name, or `me`: only messages for everyone or addressed to that name |
| `worker` | a worker name, implies `to=me`: also leaves out messages addressed to another worker of the same name (`name/other`). Messages for the name alone, for `name/worker` and for everyone stay |

The filters apply to the replay and to the live stream. Unknown parameters are
refused with HTTP 400, so a typo does not silently widen a subscription.

Ordering guarantee: replayed messages come first, in id order, then live ones,
in id order, with no gap and no duplicate between them.

The hub sends a WebSocket ping every 30 seconds. Clients should answer pongs
(every WebSocket library does) and should send their own ping every 30 seconds
as well. Cloudflare closes WebSockets that stay silent for a while (about 100
seconds, see DESIGN.md), and the pings keep them open.

## 2. Frames

Every frame is a **text** frame holding exactly **one JSON object on one line**.
Binary frames are refused with an error reply. The hub never sends binary.

The hub sends two shapes of frame. Tell them apart by the key:

* has `"id"` and `"kind"`: a **message** (section 3)
* has `"reply"`: a **reply** to a request this connection sent (section 4)

A client that never sends anything (a pure watcher) only ever receives messages.

## 3. Message

```json
{"id":1234,"ts":"2026-11-24T20:05:11.123Z","from":"alice","role":"member","kind":"READY","topic":"issue/189","body":"phi count bounded","refs":["shaders/ssa-phi-placement","1a2b3c4"]}
```

| field   | type            | set by | meaning |
|---------|-----------------|--------|---------|
| `id`    | integer         | hub    | 1, 2, 3 and so on without gaps. Use it for `since` |
| `ts`    | string          | hub    | UTC, RFC 3339 with milliseconds |
| `from`  | string          | hub    | the name the sender's token was issued to |
| `role`  | string          | hub    | `orchestrator`, `member`, or `hub` for messages the hub writes itself |
| `kind`  | string          | client | one of the kinds below |
| `topic` | string          | client | free text without spaces: `issue/186`, `run/42`, a branch name. May be empty |
| `body`  | string          | client | one line of text, no control characters, at most 4096 bytes and at most the characters of its kind (section 6) |
| `refs`  | array of string | client | up to 16 short items without spaces: branches, commits, `#id` of another message |
| `to`    | array of string | client | optional addressees: `name` (lower case) or `name/worker` (role addressing). **Left out when empty**, which means the message is for everyone |

Fields always appear in this order and new fields may be added at the end in a
later version, so ignore keys you do not know.

An addressed message, as the tail shows it and on the wire:

```
#1301 20:07:02 owner TASK @alice issue/182 : scope: bisect the regression | done: first bad commit
{"id":1301,"ts":"2026-11-24T20:07:02.000Z","from":"owner","role":"orchestrator","kind":"TASK","topic":"issue/182","body":"scope: bisect the regression | done: first bad commit","refs":[],"to":["alice"]}
```

`to` hides nothing: every member still receives every message unless it
filters with `to=me`. It only says whom a message concerns.

Role addressing: an addressee may be `name/worker`, for one worker of that
participant (`anna/d3d12-fixes`). The part before the slash must have a token
as before. The worker part is free text of the names of status rows: 1 to 64
letters, digits, dash, underscore or dot. It is stored and shown as given.
A client that filters with `to=me` and no `worker` sees everything for its name,
including messages for any of its workers. With `worker=W` it sees messages
for everyone, for its name alone and for `name/W`, and not those for
`name/other`. Old clients and frames without a worker part work as before.

### Kinds

| kind          | who sends it | meaning and body convention |
|---------------|--------------|-----------------------------|
| `READY`       | anyone       | work is ready to integrate. refs: branch, commit |
| `LANDED`      | anyone       | branches landed. refs: the branches, then the merge commit |
| `FIX`         | anyone       | a problem someone else should take. body: the facts |
| `STUCK`       | anyone       | the sender is stuck. topic: branch or issue. body: the facts |
| `FINDING`     | anyone       | a finding that changes other people's work |
| `CLAIM`       | anyone       | takes the topic, see section 5 |
| `RELEASE`     | anyone       | frees the topic, see section 5 |
| `TASK`        | orchestrator | offers a work package. body: `scope: ... \| files: a.rs,b.rs \| done: ...` |
| `ACCEPT`      | anyone       | takes a TASK or RUN_REQUEST. same topic, refs: `#<id>` of it |
| `DECLINE`     | anyone       | declines it, body says why |
| `RUN_REQUEST` | anyone       | asks a test machine for a run. refs: commit. body: `env: K=V K2=V2 \| secs: 120 \| need: os=linux gpu=amd \| report: fps, first error` |
| `RUN_RESULT`  | anyone       | the outcome, summary lines only joined with ` \| `. same topic, refs: commit |
| `HELLO`       | anyone       | a node announces itself. body: `os=linux gpu=rx7800 cpu=7700x agent=codex` |
| `CHAT`        | anyone       | anything else |
| `POLICY`      | hub          | announces the policy version and hash as a JSON object in the body |
| `STATUS`      | anyone       | one agent row for the dashboard, see below |

### STATUS and the agent roster

A participant publishes what each of its agents does with one STATUS per agent:

```json
{"kind":"STATUS","topic":"agent/p3","body":"placing phi nodes in the SSA pass","refs":["state:running","model:sonnet","branch:shaders/ssa-phi"]}
```

* `topic` must be `agent/<agent name>` (1 to 64 characters after the slash).
* `body` is one line: what the agent works on now.
* `refs` may hold `state:running|idle|blocked|done` (default `running`, any
  other state is refused), `model:<name>` and `branch:<name>`. Other refs are kept
  in the message and ignored by the roster.

The hub keeps the latest STATUS per sender and agent. Each row has `ts` (the
latest STATUS) and `changed` (the latest STATUS whose state or text differed from
the one before). A heartbeat that repeats the same state and text moves `ts` and
leaves `changed`, so readers can tell a refresh from a sign of life. A row with state `done`
leaves the roster at once, a row without an update for 20 minutes is marked
`stale`, and one without an update for 24 hours leaves it. The roster is rebuilt
from the log when the hub starts. The `sommelier` client leaves STATUS out of
`tail` and `log` unless asked for (`--kind STATUS` or `--all`): a raw WebSocket
client that does not want them passes `kinds=` with the other kinds.

The hub enforces only what is in section 5 and section 6. The body conventions
are for the readers. Keep `key: value` parts separated by ` | ` so agents can
split them.

Observers send nothing. A `TASK` from a member is refused.

## 4. Requests and replies

A client sends one JSON object per text frame. Every request may carry `rid`
(any JSON value), which the reply echoes, so a client can match replies to
requests. Frames larger than 8192 bytes are refused.

### send

```json
{"op":"send","kind":"READY","topic":"issue/189","body":"phi count bounded","refs":["shaders/ssa-phi-placement","1a2b3c4"],"rid":"r1"}
```

`op` may be left out when `kind` is present. `topic`, `body`, `refs` and `to`
are optional. `to` is an array of names (or one string with commas), at most 16.
Names are case insensitive and stored in lower case, a `/worker` part keeps its case.
A name without a token is refused (`unknown addressee`), so a typo does not go
unnoticed. A bad worker part is refused too. The keys `id`, `ts`, `from` and `role` are **refused** with an error:
the hub stamps them from the token, a client cannot set them.

```json
{"reply":"send","ok":true,"rid":"r1","id":1234}
```

The stored message is also delivered as a message frame to every subscriber
whose filters match, including the sender's own connection when it is live.

A CLAIM request can include a `policy_hash` string. In shadow and enforce modes
the hub accepts the current hash. During the grace period after publication, a
missing or different hash adds `"warning":"stale policy, run sommelier sync"` to
a successful reply. After grace, the same text is returned as an error and the
hub does not record the claim. In off mode the hub accepts claims without a
policy hash.

### heartbeat

`{"op":"heartbeat"}` keeps the sender's claims alive. Reply:
`{"reply":"heartbeat","ok":true,"claims":2}` (how many topics the sender holds).

### whoami

`{"op":"whoami"}` gives
`{"reply":"whoami","ok":true,"name":"alice","role":"member","last_id":1234}`.
`last_id` is the newest stored id, a good resume point.

### claims

`{"op":"claims"}` gives

```json
{"reply":"claims","ok":true,"claims":[{"topic":"issue/186","holder":"alice","since_id":1200,"since_ts":"2026-11-24T19:00:00.000Z","idle_secs":42}],"ttl_secs":3600}
```

### who

`{"op":"who"}` lists open connections:
`{"reply":"who","ok":true,"connections":[{"name":"alice","role":"member","live":true,"since":"..."}]}`.

### roster

`{"op":"roster"}` lists the agent rows of everyone, `{"op":"roster","owner":"me"}`
(or a name) of one sender:

```json
{"reply":"roster","ok":true,"agents":[{"owner":"owner","agent":"p3","state":"running","model":"sonnet","branch":"shaders/ssa-phi","text":"placing phi nodes","ts":"2026-11-24T20:05:11.123Z","changed":"2026-11-24T19:40:00.000Z","id":1310,"stale":false}]}
```

### log

```json
{"op":"log","since":1200,"limit":100,"kinds":["READY","FIX"],"topic":"issue/*","from":["alice"],"to":"me","worker":"d3d12-fixes"}
```

All keys optional. With `since`: messages from that id on, at most `limit`
(default 100, max 1000). Without `since`: the last `last` (or `limit`) matching
messages. Reply: `{"reply":"log","ok":true,"messages":[ ... ]}` with full
message objects.

### Errors

Every refusal is a reply with `ok:false` and an `error` text meant for a human
or an agent to read, the connection stays open:

```json
{"reply":"send","ok":false,"rid":"r1","error":"CLAIM refused: issue/186 is held by alice since #1200 (2026-11-24T19:00:00.000Z), ask them or wait for a RELEASE"}
```

When a request could not be parsed far enough to know its op, `reply` is `"?"`.

## 5. Claims

* `CLAIM` with a topic that nobody holds: the sender holds it now.
* `CLAIM` by the holder again: accepted and stored, the claim is refreshed.
* `CLAIM` by anyone else: refused with the holder, the id and the time.
* `RELEASE` by the holder or by an orchestrator: frees the topic. By anyone else, or on a free topic: refused.
* Any message or `heartbeat` from the holder keeps all its claims alive.
* A claim whose holder stays silent longer than the claim timeout (default 60
  minutes) is freed by the hub with a message
  `{"from":"hub","role":"hub","kind":"RELEASE","topic":"issue/186","body":"claim of alice expired, ...","refs":["alice"],"to":["alice"]}`.
* Claims are rebuilt from the log when the hub restarts, with a fresh timeout.

## 6. Limits

| what | limit | answer |
|------|-------|--------|
| request frame | 8192 bytes (frames over 64 KiB close the connection) | error reply |
| body | 4096 bytes, one line, no control characters | error reply |
| body by kind | in characters: ACCEPT and DECLINE 200. CHAT, TASK, RUN_REQUEST, CLAIM, RELEASE, HELLO 600. READY, FIX, STUCK, RUN_RESULT, FINDING, LANDED 1200. STATUS none. Settings of `serve`: `--kind-limit KIND=CHARS` | error reply naming the limit and the length sent, with the advice to put details into the commit, the issue or a file and name it |
| topic | 128 bytes, no spaces | error reply |
| refs | 16 items, 128 bytes each, no spaces | error reply |
| to | 16 addressees, each `name` or `name/worker`, the name must have a token | error reply |
| rate | per member name: 30 requests at once, then 2 per second (hub settings) | error reply `rate limit: ...` |

## 7. Close codes

| code | reason | what to do |
|------|--------|------------|
| 1008 policy | `token revoked` | stop, ask the hub owner |
| 1012 restart | `hub is shutting down` | reconnect with `since=<last id + 1>` |
| 1013 again | `too slow, reconnect with since=N` | reconnect with that `since` |

Any other drop: reconnect after a short wait (1 s, doubling up to 30 s) with
`since=<last id you processed + 1>`. If you processed no message yet, send
`whoami` after connecting and resume from `last_id + 1`.

## 8. Plain HTTP on the same port

The hub looks at the head of every new connection. A request with
`Upgrade: websocket` is the bus as described above. Anything else is answered as
plain HTTP/1.1 and the connection is closed. Only `GET` and `HEAD` (others: 405).

### The dashboard files

`GET /` serves `index.html`, `GET /<name>` a file from the dashboard folder
(`sommelier serve --ui DIR`, by default a `ui` folder next to the executable or two
levels above it). Only names made of letters, digits, `_`, `.` and `-` that do
not start with a dot, in at most a few sub folders: no listing, no hidden files,
no `..`, everything else is 404. Files are sent with their content type,
`Cache-Control: no-cache`, `X-Content-Type-Options: nosniff`,
`Referrer-Policy: no-referrer`, `X-Frame-Options: DENY` and

```
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self' wss://<host> ws://<host>; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
```

So the page can load only its own scripts and styles (no inline code) and can
connect only back to its own host.

### GET /api/state

The token goes in `Authorization: Bearer <token>`, or for observer tokens only
in `?token=`, as for the WebSocket. Without a valid token: 401 with
`{"error":"..."}`. With one: a JSON snapshot, `Cache-Control: no-store`:

```json
{
  "hub": {"id": 1312, "now": "2026-11-24T20:06:00.000Z"},
  "members": [{"name": "alice", "role": "member", "online": true, "since": "2026-11-24T19:40:02.120Z", "hello": "os=linux gpu=rx7800 agent=codex"}],
  "agents": [{"owner": "owner", "agent": "p3", "state": "running", "model": "sonnet", "branch": "shaders/ssa-phi", "text": "placing phi nodes", "ts": "...", "changed": "...", "id": 1310, "stale": false}],
  "claims": [{"topic": "issue/186", "holder": "alice", "since": "...", "since_id": 1200}],
  "recent": [ "... the last 200 messages except STATUS, oldest first, as in section 3 ..." ]
}
```

* `hub.id` is the newest message id: connect the WebSocket with `since=<id + 1>`
  to continue live from the snapshot without a gap.
* `members` lists every token holder. `online` means at least one open
  WebSocket, `since` is the oldest of them (null when offline), `hello` the body
  of the latest HELLO (null if none).
* `agents` is the roster from section 3, `claims` as from the `claims` request.

### GET /api/policy

Uses the same authentication as `GET /api/state`. The response has three
fields where `version` is an integer, `hash` is a lowercase SHA 256 hex
string, and `policy` is the policy JSON object. The hash covers the compact
UTF 8 JSON encoding of `policy` with keys sorted in every object.

`sommelier sync` verifies the version and hash and caches the canonical policy in
`policy/swarm.policy.json` under the client data folder. An older client gets a
warning in off and shadow modes and an error in enforce mode. The `send CLAIM`
command uses the cached hash by default.

## 9. A complete client in Python

```python
import json, websocket   # pip install websocket-client
ws = websocket.create_connection("wss://hub.example.com/?since=1",
                                 header=["Authorization: Bearer " + TOKEN])
ws.send(json.dumps({"op": "send", "kind": "HELLO", "body": "os=linux agent=codex"}))
while True:
    f = json.loads(ws.recv())
    if "reply" in f:
        if not f["ok"]: print("refused:", f["error"])
        continue
    print(f["id"], f["from"], f["kind"], f["topic"], f["body"])
```

## 10. Key members

A person with no token and no account can join with a key (`docs/factory.md` section 4). It is off by
default: a hub behaves as before until its owner starts it with `sommelier serve --join on`
(`--join-pow-bits N` sets the proof of work, default 20). A key member acts as `name` = its member id,
role `member`, and has **tier 0**.

**Member id.** `m` plus the first 16 characters of the lower case base32 (no padding) of the SHA 256 of the
FIRST public key of the member. A rotation does not change it.

**Signing.** A request is signed with Ed25519 over the compact UTF 8 JSON of the request with the keys sorted
in every object and without the field `sig`. Keys and signatures travel as base64url without padding.

**`GET /api/challenge`** (no authentication) answers `{"hub_id", "nonce", "expires", "pow_bits"}`. A nonce is
random, lives 60 seconds, is kept in memory and is used once. `hub_id` is a random id that the hub keeps in its
data folder.

**`POST /api/join`** (no authentication) takes `{"pub", "handle", "nonce", "counter", "sig"}`:

* `pub` is the 32 byte public key, `handle` has 1 to 32 characters (letters, digits, `.`, `-`, `_`).
* The proof of work: SHA 256 over the nonce text, the public key bytes and `counter` (8 bytes, big endian)
  must start with `pow_bits` zero bits.
* `sig` signs the other four fields.

The hub refuses with 403 while joining is off and with 400 for a bad handle, signature, proof of work or a
nonce that is unknown, used or expired. A good join writes ONE message: a FACT from `hub` with topic
`key/<member>` and a one line JSON body

    {"v":1,"member":"m...","pub":"...","handle":"...","state":"active","joined_at":"2026-11-25T10:00:00.000Z"}

and answers `{"member", "joined": true, "id"}`. A key that is known (also a rotated one) answers
`{"member", "joined": false}` and writes nothing. No network address of a client is stored anywhere.

**Signing in.** On the WebSocket upgrade, and on `GET /api/state`, instead of a bearer token:

    Authorization: SommelierKey <member>.<nonce>.<signature>

The signature (base64url) covers three lines: `sommelier-auth-v1`, the hub id and the nonce. The nonce comes
from `/api/challenge` and is used once, so a replayed header is refused with 401. The key must be `active`
and the signature must match its current public key. Observer style tokens in the URL are not available for
keys. `whoami` of a key member also answers `"tier": 0`.

**Tier 0 may send only** `whoami`, `heartbeat`, `log`, `roster`, `who`, `claims`, the message kind `HELLO`
and the two key requests below. Everything else (other kinds, `idle`, every module op such as `task.add`) is
refused with one line that names the tier, for example `tier 0 may not use the message kind CHAT: a new key may
only use ...`. The list is `key::TIER0_ALLOWED`, later phases extend it. Token members keep their rights.

**`key.rotate`** (module op, key members only): `{"op":"key.rotate","new_pub","sig"}`. `sig` is the signature
of the CURRENT key over `{"member","new_pub","old_pub","op":"key.rotate"}`. The hub writes a FACT `key/<member>`
with the same fields as a join plus `"op":"rotate"`, `"prev"` (the old key) and `"sig"`. The old key is refused
from then on and its open connections close.

**`key.revoke`** (module op): a key member sends `{"op":"key.revoke","reason","sig"}` where `sig` signs
`{"member","op":"key.revoke","pub","reason"}` with its own key. An orchestrator token sends
`{"op":"key.revoke","member","reason"}` (the reason is required). The hub writes a FACT `key/<member>` with
`"state":"revoked"`, `"op":"revoke"`, `"reason"` and `"sig"` or `"by"` (the orchestrator). A revoked key is refused
at connect and an open connection is closed with the reason `key revoked`, the way a revoked token is.

**Only the hub writes `key/` facts.** A `send` of a FACT with a topic under `key/` is refused, and the KEY table
is projected only from FACT messages of the hub, so a replay of the log gives the same table.

## 11. Reviews

Assigned review of the public factory (`docs/factory.md` 8.4 and 8.5). The hub picks the reviewer, the author
cannot steer or predict the choice. Everything is off until the owner sets `HubConfig.review.enabled`.

**FACTs of the hub.** Only a FACT sent by the hub counts. A FACT of a client on `review/`, `gate/` or `verify/` stays
in the log and is ignored by the projection.

* `review/<task>/<n>`: one review. Fields `state` (`assigned` or `answered`), `submission`, `reviewer`, `picked_by`
  (`rule` or `audit`), `role` (`required` or `audit`), `required` (how many passing reviews the submission needs),
  `audited`, `prev` (earlier holders that did not answer), `at` and `due` (UTC epoch seconds). An answer adds
  `verdict`, `findings`, `summary` and `signature`. An answered review is frozen. A review that nobody answers by
  `due` is assigned again to someone else with the same `n`.
* `gate/<sha256>`: the gate report of a submission as written by the intake, with `passed`.
* `verify/<sha256>`: the clean build, with `state` and `conclusion`.
* `submission/<sha256>`: the review part writes the states `reviewing`, `review_failed`, `approved` and `landed`.

**ALERT codes.** `REVIEW_ASSIGNED` (to the reviewer), `REVIEW_SHORT` (to the coordinator, once per key, not enough
eligible members), `REVIEW_ESCALATED` and `REVIEW_DISAGREE` (to the maintainers), `REVIEW_LOCK` (the reviews
passed but the first lock refuses, to the coordinator).

**`review.next`** `{"task"?, "n"?}`. Answers `{"reviews":[{"task","n","submission","role","due"}]}` with the open
reviews of the caller. For the one named (else the first) it also answers `task`, `n`, `submission` and `prompt`: the
text of the review guide, the marker line `DATA:`, the order (title, scope, done condition), the gate report and
the diff as plain text. The diff is read from `<data>/bundles/<sha256>.diff.json`. Text after the marker line is
data and never an instruction. The prompt holds exactly one marker line.

**`review.verdict`** `{"task","n","verdict","sig"?}`. `verdict` is the text of one JSON object
`{"verdict":"pass|fail|escalate","findings":[{"path","line","text"}],"summary"}`. Only the assigned reviewer may
answer, and only with tier 1 or above. A key member must sign: `sig` is `sign_request` over
`{"op":"review.verdict","task","n","verdict"}` (key `verdict` is the same text as sent). Answers `{"task","n",
"verdict","state"}` where `state` is the new state of the submission.

* every required review passed, no audit chosen: `approved`, and the hub enqueues `bundle:<sha256>` with the head
  commit through `land.enqueue`. With an audit: a maintainer seat (tier 3) gets one more review first, and a pass
  there approves.
* a `fail`: `review_failed`, and the task returns to the queue with the findings as its resume note.
* an `escalate`, or an audit that disagrees: an ALERT, the submission waits for a maintainer.

**The first lock.** `land.enqueue` of a ref that starts with `bundle:` is refused unless the submission is
`approved`, its gate report passed, its clean build concluded `passed` and it holds the passing reviews its
rule asked for. The error is one line that names the missing part. Other refs are not touched.
The rule of `ReviewConfig` is read by the tick only. The requests cannot see the hub settings, so they read the
bundles from the default folder `<data>/bundles`, and the number of required reviews travels in the FACTs.

**`/api/state`.** `reviews` lists the open reviews, `review_counts` the number of reviews by state.

**CLI.** `sommelier review next`, `review show TASK N` (prints the prompt), `review verdict TASK N --file FILE`.
