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
80 hex characters: its expiry time, a random part and an HMAC of both under a secret of the hub (the file
`challenge_secret` in the data folder, never logged or sent). It lives 60 seconds and is used once. The hub keeps
nothing for a nonce it hands out, only the nonces that were USED until they expire, and only after a valid
signature, so a flood of challenges fills nothing. `hub_id` is a random id that the hub keeps in its data folder.
A client pins the `hub_id` per hub address in `member.hubs` beside its key file at the first join or sign in and
refuses an answer with another id. To accept a moved hub on purpose, delete the line of that address in the file.

**Joining off means off.** While joining is off (the default) the hub serves no challenge (`GET /api/challenge`
answers 403), refuses `POST /api/join` with 403 and accepts no key sign in, also not for keys that joined earlier.
A hub with joining off behaves as before the key feature. Bearer tokens are not affected.

**`POST /api/join`** (no authentication) takes `{"pub", "handle", "nonce", "counter", "sig"}`:

* `pub` is the 32 byte public key, `handle` has 1 to 32 characters (letters, digits, `.`, `-`, `_`).
* The proof of work: SHA 256 over the nonce text, the public key bytes and `counter` (8 bytes, big endian)
  must start with `pow_bits` zero bits.
* `sig` signs the other four fields.
* A client refuses a `pow_bits` above 40 and a member id in the answer that is not the one its key makes.

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

**Tier 0 may send only** `whoami`, `heartbeat`, `log`, `roster`, `who`, `claims` and the two key requests below
(and the typed requests that later phases add to `key::TIER0_ALLOWED`). A key member sends no message kind, in
particular no `HELLO`. It has the trust `tier0` in the MEMBER table, never a CAPACITY, MACHINE or ROLE_HELLO row,
and no slot or lease. Slot counts of every HELLO are cut to 64 per class, 16 classes and 256 in all. Everything else (other kinds, `idle`, every module op such as `task.add`) is
refused with one line that names the tier, for example `tier 0 may not use the message kind CHAT: a new key may
only use ...`. The list is `key::TIER0_ALLOWED`, later phases extend it. Token members keep their rights.

**`key.rotate`** (module op, key members only): `{"op":"key.rotate","new_pub","sig"}`. `sig` is the signature
of the CURRENT key over `{"member","new_pub","old_pub","op":"key.rotate"}`. The hub writes a FACT `key/<member>`
with the same fields as a join plus `"op":"rotate"`, `"prev"` (the old key) and `"sig"`. The old key is refused
from then on and its open connections close. The client keeps the new key in `member.key.next` until the hub
confirms it. When the answer is lost, the next sign in tries that key and replaces `member.key` when the hub takes it.

**`key.revoke`** (module op): a key member sends `{"op":"key.revoke","reason","sig"}` where `sig` signs
`{"member","op":"key.revoke","pub","reason"}` with its own key. An orchestrator token sends
`{"op":"key.revoke","member","reason"}` (the reason is required). The hub writes a FACT `key/<member>` with
`"state":"revoked"`, `"op":"revoke"`, `"reason"` and `"sig"` or `"by"` (the orchestrator). A revoked key is refused
at connect and an open connection is closed with the reason `key revoked`, the way a revoked token is.

**Only the hub writes `key/` facts.** A `send` of a FACT with a topic under `key/` is refused, and the KEY table
is projected only from FACT messages of the hub, so a replay of the log gives the same table.

## 11. Test network

Testers need no model and no lease, so run orders are their own small table and never touch the task queue.
The part is off until the owner sets `HubConfig.test.enabled`. While it is off every request below answers
`the test network is switched off on this hub`. A request in the first moments of a hub, before its first tick,
answers `the test network is starting: try again in a few seconds`.

**Run orders.** The table RUN_ORDER (id, build, channel, profile, scenario, needs, wanted, state, created_at) is
projected from FACTs of the hub with topic `runorder/<id>`. The body is one line of JSON:
`{"v":1,"id","build","channel","profile","scenario","needs":{"os"?,"gpu"?},"wanted","state","created_at"}`.
The first FACT of an id makes the row. Later FACTs change only the state: `open` to `settled` or `closed`,
`settled` to `closed`. `closed` is final.

**`test.order`** (orchestrator token): `{"op":"test.order","build","channel","profile","scenario","needs"?,"wanted"?}`.
`build` is a commit of 7 to 64 lower case hex digits, `channel` is `main` or `candidate`, `needs.os` is `windows`
or `linux`, `needs.gpu` is a card name that the normalised GPU of the tester must contain, `wanted` (1 to 50,
default the quorum) is the number of independent owners that settle the order. Answers `{"id","msg"}`. The same
order twice while it is open is refused. **`test.close`** (orchestrator token): `{"op":"test.close","id"}`.

**`test.next`** (any member, tier 0 too): `{"op":"test.next","machine":<machine class>,"profiles":[ids]}`. The
machine class is `{"os","os_build","gpu","gpu_memory_mb","driver","memory_mb"}`. The answer is
`{"order":{"id","build","channel","profile","scenario","wanted"}}` for the best open order: its needs fit the
machine, its profile is one of `profiles`, the member has not reported on it, and it has the fewest reports so far
(then the oldest). Without a match the answer is `{"order":null,"reason":"nothing to test now"}` and the client
exits with code 6. A member may hold `test.max_handed_per_member` handed orders that it has not answered. The
answer says nothing about other testers.

**`report.send`** (any member, tier 0 too): `{"op":"report.send","report":{...}}`. The report is the one of
`report::types::Report`. A key member signs it: `sig` is the signature (base64url) of the member's current key
over the canonical form of the report without `sig` (the compact JSON with sorted keys). A token member needs no
signature. `report.member` must be the sender. The run order must exist and be open, and build, channel,
profile and scenario must match it. One report per member and order. Then the report is checked (privacy,
limits, a log excerpt of at most 2048 bytes by default) and the hub writes the FACT `report/<run>`. A message
body of the hub is at most 4096 bytes, so a report must stay below that. Answers `{"run","id"}`.

**Only the hub writes `report/` and `runorder/` facts.** The tables are projected only from FACT messages of the
hub (role `hub`), so a FACT of a client in the log changes nothing, and a replay of the log gives the same
tables. `/api/state` keeps `matrix` and gains `run_orders`, the open orders with the number of their reports.

**The tick** settles an order when the reports of one machine class agree at `wanted` independent owners
(distinct members in distinct address blocks), closes an order older than `test.order_ttl_secs`, and raises one
alert MATRIX_REGRESSION with `data.last_good` and `data.first_bad` when a profile and machine class goes from
`works` on an older build to `fails` on a newer one. The order of builds is the order of their first run orders.

**`sommelier test`** runs one round: machine class, `test.next`, the check of the build with `gh attestation
verify` (no flag turns it off), the check of the title against the profile, the scenario through the run script
(`swarm/run-title.sh`), the report, its signature, `report.send`. A failed build or title check sends an
inconclusive report for the order that was taken and runs nothing. `--loop` repeats while the pause file does
not exist. See `sommelier test --help`.

## 12. Reviews

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

## 13. Submissions

A member that holds the lease of an order sends its work as one git bundle. Everything here is off until the
owner switches the intake on (`intake.enabled`). A hub that has it off answers `503` on the upload and `404` on
the reads below.

**Upload.** `PUT /api/submit/<task>/<attempt>?sha256=<hex>[&sig=<signature>]` with the bundle as the body.

* The caller signs in with a token (`Authorization: Bearer`) or with a key (`Authorization: SommelierKey`).
* It must hold the active lease of that task and attempt, else `403` (no lease, another member) or `409` (wrong attempt).
* A key member must send `sig`: the base64url signature, made with `sign_request`, over
  `{"op":"submit","task":"<task>","attempt":<attempt>,"sha256":"<hex>"}`. A missing or wrong one is `401`.
* The body is at most the cap of the tier of the caller (`413` otherwise). The hub reads no more than the cap of
  the highest tier on this path and applies the own cap of the caller after it knows who it is.
* `sha256` is the SHA 256 of the body in lower case hex. A body with another hash is `400`, a body that is no git
  bundle with exactly one ref is `400`.
* The file is stored as `<store>/<sha256>.bundle` through a temporary file and a rename. The hub writes the FACT
  `submission/<sha256>` with the state `received` and answers `{"ok":true,"submission","task","attempt","state","bytes","known"}`.
  The same bundle sent again by the same member for the same order answers with the known state (`"known":true`)
  and writes nothing. Another member or order with that hash is `409`.

**States.** `received`, then `gated` or `gate_failed`, then `verifying`, then `verified` or `verify_failed` (the
later states belong to the review). Every change is a FACT `submission/<sha256>` of the hub, so the SUBMISSION
table tells where each patch stands. `GET /api/state` has `submissions`: `{"counts": {<state>: n}, "newest": [<the 20
newest rows>]}`. The request `submit.status` lists the caller's own submissions (a tier 0 member may use it).

**Static gates (S1).** A worker thread unpacks the bundle against the reference repository of the hub with the
base of the lease and runs the gates, never a build. The parsed diff is stored as `<store>/<sha256>.diff.json` and
the report as `<store>/<sha256>.gate.json`. The hub writes the FACT `gate/<sha256>` (the report, the blocking
findings first, cut to fit one message), then `submission/<sha256>` with `gated` or `gate_failed`. A failed gate
returns the order to the queue with the blocking findings as the resume note, the way `task.requeue` does.

**Clean build (S2).** With `verify.enabled` the hub picks `gated` submissions by the tier shares, starts the
workflow with a `workflow_dispatch` (inputs: the hash, the order, the base and the public address of the hub) and
reads the conclusion of the run through the GitHub API. Each step is a FACT `verify/<sha256>` with `state`
(`dispatched`, `running`, `done`), `run_id`, `conclusion` (`passed`, `failed`, `cancelled`, `timed_out`, `no_run`),
`failing_step` and `minutes`, plus `order`, `dispatches` and `started_ms` so a restart can go on. A dispatch that
shows no run is retried once. The submission moves to `verifying`, then `verified` or `verify_failed`. A failed
build returns the order with the failing step as the resume note. `verify.retry` (orchestrator only,
`{"op":"verify.retry","sha256"}`) puts a `verify_failed` submission back to `gated`.

**Reads for the runner (no credentials, the hash is the key).**
`GET /api/bundle/<sha256>` serves the bundle of a submission that passed the gates, else `404`.
`GET /api/order/<task>/done_when` answers `{"task","base","scope":[...],"done_when"}` for an order that has such a
submission, else `404`. The base is the base of that submission.

**Only the hub writes** `submission/`, `gate/` and `verify/` facts. The tables are projected only from FACT
messages of the role `hub` whose body names the hash of the topic, so a replay of the log gives the same rows.

## 14. Front door

The hub can be reached by anyone, so it limits what one source can take, and it can tell two keys of one owner
apart from two owners. Every part is off until the owner turns it on in the `door` settings of the hub. A hub
that upgrades behaves as before.

**Address blocks.** The hub never stores a network address. It reduces the address of a client (the /24 of an
IPv4 address, the /48 of an IPv6 address) to an opaque block: an HMAC under a key derived from the hub secret,
cut to 16 hex characters. The raw address is used for that one computation in memory. It is not stored, logged
or sent, also not in an error text. The side store `door.db` in the data folder (never part of `state.db`, never
served) keeps `member`, `block`, `first_seen` and `last_seen` for the members that joined or connected. Rows
older than `keep_days` are dropped on the tick. With `keep_days` at 0 nothing is kept. Two members with the same
block count as one owner for reviews and for test reports.

**Which address.** The socket peer. Behind a tunnel every peer is the loopback address, so `trust_header` names
a header (for example `CF-Connecting-IP`) that carries the client address. It is honoured only when the socket
peer is the loopback address, from any other peer it is ignored. With a list of addresses the last one counts.

**Limits.** Every refusal names the limit and the time to retry. WebSocket upgrades and plain requests answer
`429`, a join answers `429` with `{"ok": false, "error": ...}`. Leases that are held keep running.

| Setting | Limit | Refusal starts with |
|---------|-------|---------------------|
| `joins_per_block_per_hour` | tries of `POST /api/join` per address block and hour | `join limit:` |
| `conns_per_member` | open connections of one member | `connection limit:` ... per member |
| `max_conns`, `reserved_share` | all open connections. The share `reserved_share` of them is never filled by tier 0 or anonymous readers | `connection limit:` |
| `public_reads_per_block_per_min` | connections and state reads of the public reader per block and minute | `public read limit:` |

A member above tier 0 is never refused because tier 0 or anonymous connections fill the hub. When more than
`join_flood_refusals` joins were refused within `join_flood_window_secs`, the hub writes ONE alert
`alert/JOIN_FLOOD/join` per window. Its body has the counts and no address.

**Public read.** With `public_read` on, a WebSocket upgrade or `GET /api/state` with no `Authorization` header
and no `token` query acts as an anonymous observer. A request with a wrong token is still refused with 401. The
observer counts against the global cap as tier 0 and has its own rate per block. It can send nothing: every
frame it sends is answered with an error. It is not listed in `who` or in the members of `/api/state`.
What it gets is redacted: no FACT with a topic under `quota/` or `spend/`, on the socket and in `recent`, and in
`/api/state` no `quota` object, and each entry of `pools` has only `pool`, `window`, `state` and `resets_at`
(no percent values, no leases, no forecast). Members with a token or a key see what they saw before.

## 15. Standing

A key member earns its tier from facts. Everything below is off until the owner turns it on (`HubConfig.stamp.enabled`).
With it off, every key member stays at tier 0 and the `stamp.*` requests answer `standing is switched off on this hub`.

**Stamps.** The hub writes a FACT `stamp/<member>` for each rule and each request of a maintainer. The body is one
line of JSON: `v` (1), `subject`, `issuer` (`hub` for a rule, the maintainer for a request), `kind`, `evidence`
(`msg`, and `commit` and `ref` when there are any), `at` and, for a maintainer, `cause`. Each rule runs on the tick
and writes one stamp per kind, subject and evidence, so a restart or a replay writes nothing twice.

| Rule | Stamp | For |
|------|-------|-----|
| a submission reached `landed` | `landed` | its author |
| a review answered `pass`, its submission landed at least `confirm_days` ago and was not reverted | `review_confirmed` | the reviewer |
| a report agrees with a settled cell of the matrix, the blocks come from `door::block_of` | `run_agreed` | the tester |
| a report contradicts a settled cell | `run_outlier` | the tester |
| a submission was `rejected` after another one of the same task landed | `superseded` | its author, neutral |

**Tiers.** On the tick the hub computes `stamp::tier` for every member that has a KEY row, from its key state, its
join time and its stamps. A tier that differs from the TIER row is written as a FACT `tier/<member>` with `tier`
(-1 means no tier at all), `reasons` and `at`. The reasons make every promotion and demotion readable in the log.
A listed maintainer is tier 3. A grant by a maintainer counts as the linked account that tier 2 needs. Only the hub
writes `stamp/` and `tier/` facts: a `send` of a FACT with such a topic is refused with `written by the hub only`.

**Hub only topics.** One list (`hub::HUB_ONLY_TOPICS`) names the prefixes that no client may write as a FACT: `key/`,
`submission/`, `gate/`, `verify/`, `review/`, `report/`, `runorder/`, `stamp/`, `tier/` and `land/bundle:`. A `send` is refused
with `written by the hub only`. Each projection also checks the sender role. A landing row for `bundle:<sha256>` reaches
the LANDING table only through `land.enqueue` and its lock: a READY, or a FACT of another sender, adds none.

**What a tier may use**, decided in one place (`key::tier_allows`) and read at the time of each request, so a
demotion works at once. Token members are untouched.

| Tier | Requests and kinds |
|------|--------------------|
| none (-1) | `whoami` |
| 0 | `whoami`, `heartbeat`, `log`, `roster`, `who`, `claims`, `key.rotate`, `key.revoke`, `submit.status`, `report.send`, `test.next`. With `tier0_coding` on also `task.next`, `task.work`, `task.checkpoint`, `task.done`, `task.stuck`, `task.decline`, `task.heartbeat` |
| 1 | all of tier 0 with the order requests, the kinds `CHAT`, `FINDING`, `FIX`, `READY`, `STUCK` and `HELLO`, and `review.next`, `review.verdict` |
| 2 and 3 | what a member with a token may do |

`whoami` of a key member answers its current `tier`. A refusal is one line that names the tier, for example
`tier 1 may not use the message kind CLAIM: ...`.

**Requests of maintainers.** A maintainer is an orchestrator token, or a token name or member id in
`stamp.maintainers`. Anyone else gets `... is for maintainers`. Each is a module op with a JSON body and answers `ids`
(the messages written).

| Op | Body | Writes |
|----|------|--------|
| `stamp.revert` | `submission`, `cause` | `reverted` for the author of a landed submission and `review_missed` for each reviewer that passed it |
| `stamp.grant` | `member` | `grant_tier2`, the evidence is the KEY fact of the member |
| `stamp.clear` | `member`, `stamp` (the id of a penalty stamp) | `cleared` that names the penalty |
| `stamp.intent` | `member`, `cause`, optional `msg` | `intent`, then revokes the key through `key.revoke`. The answer has `revoked` |

An open penalty drops the earned tier by one, `cleared` closes it. A grant alone does not give tier 2 and the facts
of tier 2 alone do not either.

**`/api/state`.** `standing` has one row per member with a stamp or a key. A hub with standing on adds to a key
member's row `tier`, `reasons`, `missing` (what the next tier still needs) and `open_penalties` (`id` and `kind`,
the ids that `stamp.clear` takes). `sommelier standing [member]` prints it, `standing grant|clear|revert|intent`
sends the requests above.
