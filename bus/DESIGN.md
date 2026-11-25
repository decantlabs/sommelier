# sommelier design notes

## Decisions

**One hub, one log.** A single process owns an append only JSON lines file
(`bus.jsonl`). Every message gets the next id, is written, applied to the claim
table and broadcast while one lock is held. So ids have no gaps, log order is
delivery order, and "replay from id N" is exact. A crash at worst loses the
line being written, which the hub closes off with a newline on the next start.

**WebSocket, text frames, one JSON object per frame.** Anything that can open a
WebSocket can join, including harnesses that treat each frame as an event. No
custom framing, no binary, no compression negotiation needed.

**The hub stamps identity.** `id`, `ts`, `from` and `role` come from the hub and
the token. A client that sends one of these keys is refused outright instead of
being silently corrected, so a broken client notices.

**Tokens in a file the owner edits.** `sommelier token add|revoke|list` edit
`tokens.json` directly, the running hub rereads it when it changes (checked every
2 s) and closes connections of revoked tokens. No admin API over the network, so
there is nothing to attack there. Tokens are 192 random bits. The file keeps a
salted SHA 256 of each token, never the token, which is printed once at `add` or
`rotate`. With that much randomness a fast hash is enough, no password
stretching. Files of the first version with plain tokens are converted by the
next token command.

**Header tokens, URL tokens only for observers.** URLs end up in proxy logs, so
a token that can write must travel in the `Authorization` header. A plain
WebSocket monitor that cannot set headers gets a separate observer token.

**Replies only to whoever asked.** A pure watcher never sees request replies,
its stream contains messages only.

**Replay by reading the file.** Replay and `log` scan the log file instead of
keeping history in memory. At a few hundred bytes per message and a team of
humans and agents this stays fast for years of use (a million messages is a few
hundred MB and a sequential scan). An index can come later if needed.

**Claims are derived from messages.** CLAIM and RELEASE are ordinary messages,
so the claim table can always be rebuilt from the log. Liveness is "the holder
said anything recently" (any message or a `heartbeat`). The hub announces
expiry as a RELEASE from `hub`, so watchers see it like any other release.

**Rate limit per name, not per connection.** A token bucket per member name
(30 at once, then 2 per second by default) covers all of that member's
connections together, and counts every request, not only sends.

**No TLS in the hub.** The hub listens on 127.0.0.1. TLS, DNS and exposure are
the tunnel's job (Cloudflare Tunnel, Tailscale, a reverse proxy). The client
does speak `wss://`, with rustls and the webpki root certificates built in, so
it works the same on Windows and Linux without system OpenSSL.

**Pings both ways every 30 s.** Cloudflare's WebSocket documentation says it
closes connections with no data in either direction for some time and gives no
number. Community and third party sources consistently name 100 seconds for
non Enterprise plans. 30 s stays well below that. Cloudflare also says it may
restart servers on releases, which drops WebSockets: the client therefore
reconnects by itself and resumes from the last id it saw.

**Few dependencies.** `tokio`, `tokio-tungstenite`, `futures-util`, `serde`,
`serde_json`, `getrandom`, and `rustls` for `wss://`. No CLI parser, no date crate.

## Verified behind a Cloudflare quick tunnel

On 2026-11-24 a hub on 127.0.0.1 behind `cloudflared tunnel --url` (version
2026.11.2) carried `send`, `tail --since` with replay, claims and a
RUN_REQUEST and RUN_RESULT over `wss://hub.example.com`. A gotcha found
on the way: if `%USERPROFILE%\.cloudflared\config.yml` exists (it does once a
named tunnel is set up), a quick tunnel reads its ingress rules and answers
every request with 404. Start the quick tunnel with an empty config file:
`cloudflared tunnel --config <empty.yml> --url http://127.0.0.1:7300`.

## Work queues, policy and scheduler

Work queues, policy and the scheduler are now planned, see [docs/architecture.md](../docs/architecture.md). A queue gives the hub one place to assign work and avoid double work. Policy lets the hub catch wrong briefs and route work to idle capacity.

The first policy step gives the hub a typed, versioned file. A hash identifies the policy a client used when it claimed work. The owner publishes locally. The hub archives versions and tells clients when to sync. The shipped policy starts in off mode.

## Deliberately left out

* **File transfer and binary frames.** Code travels through git branches, the
  bus carries branch names and commit ids.
* **Private messages and per topic permissions.** Everyone with a token reads
  everything. Keep the member list small instead.
* **Message edits and deletes.** The log is append only. Corrections are new messages.
* **Delivery acknowledgements.** The log records bus messages. ACCEPT and CLAIM
  record decisions about work. Work queues, policy and the scheduler are now
  planned, see [docs/architecture.md](../docs/architecture.md).
* **Federation between hubs, clustering, high availability.** One hub per team.
* **TLS termination and an admin web UI.**
* **Structured bodies.** The body is one line of text with ` | ` separated
  `key: value` conventions that agents read easily. A typed schema per kind can
  come once the conventions settle.

## Later option: serverless peer to peer transport

The hub could be replaced by [iroh](https://docs.rs/iroh) with
[`iroh-gossip`](https://docs.rs/iroh-gossip) topics, which would remove the need
for one machine and a tunnel.

What I checked in the docs on 2026-11-24: iroh 1.5.0 is the current release.
Each endpoint is identified by its public key, connections are QUIC with TLS,
peers first connect through a relay and then try a direct connection with hole
punching, and by default the relays run by number 0 (the iroh company) are
used. `iroh-gossip` 0.103.0 builds on iroh 1.x, implements HyParView and
PlumTree, identifies a topic by 32 bytes, and its `subscribe` takes a list of
bootstrap peers that the application has to supply. A maximum message size
is configurable.

What I did not verify: the numeric message size limit, how stable the gossip
API is (it is still 0.x while iroh is 1.x), whether gossip gives any ordering
or delivery guarantee (it is epidemic broadcast, so assume neither), and how
replay for a peer that was offline would work. That last point is the real
cost: without a hub there is no single log, ids would no longer be dense, and
claims would need a conflict rule. It fits a later version where every node
keeps its own log and a designated node acts as the claim authority.
