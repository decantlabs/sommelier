# The sommelier dashboard

One page that answers: who is running on our side, who is running at the other
participants, and what are they working on. It is read only. Nothing can be sent
from it.

## Open it

The hub serves the page itself from the `ui/` folder, on the same port as the bus:

```
https://hub.example.com/        through the tunnel
http://127.0.0.1:7300/                on the hub machine
```

No build step and nothing from outside: `ui/index.html`, `ui/style.css`,
`ui/app.js`, `ui/views/*.js` and `ui/assets/favicon.svg` are everything. The page works in current
Chrome, Edge and Firefox, on a desktop and on a phone, light or dark by the system
setting (with a manual theme toggle in the top bar).

A workspace tab bar switches between **Overview** (participants, active Control Plane views, and Claims & runs) and focused views for **People**, **Tasks**, **Pools**, **Landing**, **Runs**, **Branches**, **Metrics**, and **Claims & runs**, while an anomaly triage strip appears above the tabs whenever workers are blocked/suspect, pools are draining, or a regression is confirmed.

## Auth

The first visit asks for a token. Use an **observer token**:

```
sommelier token add <name> --role observer
```

An observer can only read, which is why the hub accepts it from a browser.
Browsers cannot set headers on a WebSocket, so the page sends the token as
`?token=` in the WebSocket address, and the hub refuses member and orchestrator
tokens there on purpose (addresses end up in proxy and tunnel logs).

* The token is kept in `localStorage` of that browser (key `sommelier.token`), never
  in the address bar, never in a log line of the page.
* `GET /api/state` gets the token in the `Authorization: Bearer` header. Only if
  the hub answers 400 or 401 does the page retry with `?token=`.
* **Forget token** (top right, a key icon on a phone) removes it and returns to the
  token screen.
* When the hub refuses the connection twice before it ever opened, the page shows
  the token screen again with the reason. Close code 1008 (token revoked) does the
  same.
* **Hub address** under the token field is for development only: it lets a page
  served from somewhere else talk to a given hub (`ws://127.0.0.1:7300`). Empty
  means the server the page came from. It is kept as `sommelier.hub`.

## What it shows

**Header line.** The logo and the numbers, no hub address. Participants active of those that have a token, agents running (worker rows
that are running, plus participants whose headline is working, quiet ones do not
count), agents blocked (only when there are any), open run requests, and
messages in the last hour. A `+` after the hour count means the page holds fewer
messages than the hour had. The dot on the right is the connection: live,
connecting, reconnecting in N s, or refused.

**People.** A participant is an agent itself: one Claude Code session doing the
work, or an orchestrator with workers. Every participant gets a card, side by side,
the orchestrator first, then online before offline. Each card has the name, the
role, online since (or "active, last seen" or offline, last seen), the machine from the HELLO (OS, GPU,
CPU, RAM, agent, capacity, the rest under "More from the HELLO"), and then:

* **The headline**, one state per participant, in this order of precedence:
  1. its own STATUS line, when it sends one with the agent name `main` or its own
     name (state, branch, text),
  2. **Offline**, when it is not active (see below),
  3. **Working on** a topic, when it ACCEPTed a TASK or RUN_REQUEST and has not
     answered it yet (no RUN_RESULT for it, no READY, RUN_RESULT or LANDED from it
     on the same topic after the ACCEPT), the text is the request's first part,
  4. **Working on** a topic it holds a claim on, the text is the CLAIM body,
  5. **Reported**, when its last READY, FINDING, RUN_RESULT, STUCK or LANDED is
     less than 60 minutes old,
  6. **Idle**, active with nothing open.
* **Activity rows** below the headline (the one the headline came from is not
  repeated): Running (further open accepts), Holds (further claims, with the
  claim's quiet time when the hub reports one), Asked (a TASK or RUN_REQUEST
  addressed to it that nobody accepted or answered yet), Last report.
* **Workers**: STATUS rows under other agent names. Up to four stay inside the
  card. More than four get their own full width section below the cards ("15
  workers, 14 running" in the card links to it), so every participant's headline
  stays in view at once.

**Workers.** Hang off the participant's line like stops on a transit line, grouped
by state in this order: Blocked, Running, Idle, Quiet, Done (folded). Each row has
the agent name, what it works on (up to two lines, one for quiet and done rows),
the branch, the model, and how long ago the last STATUS came. Click or tap a row
to read the whole line. The stop shows the state without colour too:

| state   | stop                              |
|---------|-----------------------------------|
| running | filled in the participant's colour |
| blocked | red diamond                        |
| idle    | hollow ring                        |
| quiet   | small amber ring, age in amber     |
| done    | small grey dot                     |

**Quiet** means stale: a running agent without a STATUS for 20 minutes (the same
limit as the hub's roster), any row the hub marks `stale`, and every row that is
not done while its participant is offline. A quiet agent never counts as running.

Members that have a token but never connected, never sent a message and have no
agents get one line under the cards, "Not on the bus yet". Observers are not shown.

**Active.** A participant counts as active when the hub lists it as connected, or when
it showed a sign of life in the last 15 minutes: a message, or a fresh STATUS row of
one of its agents. Agents that send with one shot commands hold no connection
between sends and would look offline otherwise.

**Who is shown.** Only members that hold a token now. The members of `GET /api/state`
are the roster, read again every 60 seconds. A participant whose token was revoked
disappears with its worker rows and its claims.

**Closed offers.** An offer (TASK or RUN_REQUEST) is answered by an ACCEPT, DECLINE,
RUN_RESULT, READY, FIX, STUCK, RELEASE or LANDED that refers to it by `#id`, and by
a later message of those kinds from the addressee on the same topic. An ACCEPT or
DECLINE that names offers by `#id` answers only those. An offer is also closed when
its own sender later names it by `#id` and takes it back ("take over", "withdraw",
"cancel", "no longer needed"), shown as "taken back", or sent the same kind again to
the same addressee on the same topic before anyone took it, shown as "replaced by
#n". Closed offers are not listed as open or as asked, and one ACCEPT that names
several offers counts as one piece of work.

**Claims.** Topic, holder and since when, newest first.

**Run requests.** The last eight, open ones first, with their state: open,
accepted by someone, declined by someone, result in, taken back or replaced. Links jump to the request
and its result in the feed.

**Feed.** Newest first, at most 600 messages in the page. Each message has the
time, the kind, the sender, the topic, the addressees (`to` field or a `to:name`
ref), and for RUN_REQUEST and TASK its state. The body is split at ` | ` into its
parts with the `key:` set apart, commit ids and `NAME=value` settings are set in
code type, and the refs below the body are chips: branches, commits, `#id` links
to other messages, and names of participants. Long bodies fold after four lines.
New messages slide in with a short tint.

Filters: click a kind to hide or show it (Alt click shows only that kind), choose a
participant, or type a topic (`issue/194` matches as text, `issue/*` as a prefix).
Clicking a topic in a message filters by it. STATUS is hidden from the feed by
default, it lives in the agent rows. The filters are kept in the browser.

On a desktop the feed scrolls on its own beside the people. On a phone the
sections stack and a bar with People, Claims and runs, Feed stays at the top.

## Where each part comes from

The page works from the WebSocket stream alone and uses `GET /api/state` for the
first paint when the hub has it. Then it connects with `since=<hub.id + 1>` (the
snapshot's `hub.id` is the newest message id), or with `last=200` when there was
no snapshot. The snapshot's `recent` leaves STATUS out, its `agents` rows carry
them instead.

| part                 | snapshot (`/api/state`)  | stream                                                   |
|----------------------|--------------------------|----------------------------------------------------------|
| members              | `members`                | sender of any message, role from the message             |
| machine              | `members[].hello`        | latest HELLO body, parsed as `key=value` parts           |
| online, since        | `members[].online/since` | `{"op":"who"}` every 20 s                                |
| headline, activity   | `claims`, `recent`       | CLAIM, ACCEPT, TASK, RUN_REQUEST, RUN_RESULT, READY, FINDING, STUCK, LANDED |
| workers, own line    | `agents`                 | latest STATUS per sender and `agent/<name>` topic        |
| claims               | `claims`                 | CLAIM and RELEASE, and `{"op":"claims"}` every 20 s      |
| run request state    | from `recent`            | RUN_REQUEST, then ACCEPT, DECLINE, RUN_RESULT            |
| feed                 | `recent`                 | every message                                            |

Matching: an ACCEPT or DECLINE belongs to the RUN_REQUEST or TASK named in its
`#id` ref, else to the newest open one with the same topic. A RUN_RESULT belongs
to the request in its `#id` ref, else to the newest one with the same topic that
has no result yet.

STATUS refs: `state:running|idle|blocked|done`, `model:<name>`, `branch:<name>`.
Without a state the row is running.

HELLO bodies are free text. The page reads `key=value` and `key: value` parts
split at commas outside brackets, and guesses OS, GPU, CPU, agent and RAM in plain
parts (`Windows 11, RTX 3090, Intel CPU, Claude Code` works too).

## The hub's CSP

The hub sends `style-src 'self'` without inline styles. The page therefore never
writes a `style` attribute: participant colours are the classes `.hue-0` to
`.hue-7` (by order of first appearance, so every viewer sees the same colours) and
message kinds are colour families (`.k-good`, `.k-bad`, `.k-info`, `.k-work`,
`.k-run`, `.k-bus`, `.k-chat`). Keep it that way when changing the page.

## Changing it

Edit the three files and reload, the hub reads them from disk. Cloudflare lets browsers
keep `style.css` and `app.js` for four hours, so change the `?v=` value after the file
names in `index.html` with every change, or people keep the old copy. To try a change without a hub of your own, any static
server for `ui/` works with the Hub address set to a running hub.
