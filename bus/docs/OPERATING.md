# Running the hub

For the owner of the hub (owner). Everything here runs on the Windows PC that hosts the hub. All commands are PowerShell and start in the sommelier folder.

## What runs where

* `sommelier serve` listens on `127.0.0.1:7300`. It accepts nothing from the network directly.
* `cloudflared tunnel run sommelier` connects out to Cloudflare and forwards `wss://hub.example.com` to `http://127.0.0.1:7300`. The ingress rule lives in `%USERPROFILE%\.cloudflared\config.yml`.
* TLS ends at Cloudflare. The hub itself speaks plain WebSocket on the loopback address.
* The data folder is `%LOCALAPPDATA%\sommelier`. It holds:

| File | What it is |
|---|---|
| `bus.jsonl` | The log. One message per line as JSON, append only, never rotated. |
| `tokens.json` | Every member with role and a salted hash of the token (never the token itself). Keep it private all the same. |
| `cloudflared.log` | The tunnel output (written by `start-hub.ps1`). |
| `hub.pids` | Process ids of the running hub and tunnel, removed on stop. |

## Build

```powershell
cargo build --release -j 4
```

`start-hub.ps1` takes `target\release\sommelier.exe` when it exists and falls back to `target\debug\sommelier.exe`, then to a `sommelier` on the PATH. The client examples below write plain `sommelier`: run `cargo install --path .` once, or write `.\target\release\sommelier.exe` instead.

## Start and stop

```powershell
powershell -ExecutionPolicy Bypass -File scripts\start-hub.ps1
```

It starts the hub with the data folder above, waits until port 7300 answers, starts the tunnel, and prints the address to hand out (`wss://hub.example.com`). The hub prints a line when a listener connects (`+ name (role)`) or leaves (`- name`). One shot commands such as `sommelier send` are not printed. Ctrl+C stops the hub and the tunnel together.

Options: `-NoTunnel` for a local hub only, `-Data DIR`, `-Listen ADDR`, `-Tunnel NAME`, `-PublicUrl URL`.

The readiness probe of the script opens a plain TCP connection once, so the hub prints one harmless `handshake refused: ... Handshake not finished` line right after the start.

From another window:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\stop-hub.ps1
```

Stopping is safe at any moment. Each message is one appended line, and the hub repairs a half written last line on the next start. Clients running `sommelier tail` reconnect by themselves with backoff (1 s doubling up to 30 s) and continue from the last id they saw, so nothing is lost across a restart.

Check the whole public path (tunnel, TLS, hub, token, send, live stream) with a member or orchestrator token:

```powershell
$env:SOMMELIER_TOKEN = "<your token>"
powershell -ExecutionPolicy Bypass -File scripts\check.ps1
```

The script reads the token from the environment variable named by `-TokenVar` (default `SOMMELIER_TOKEN`), sends one CHAT on the topic `check` and waits until a `sommelier tail` sees it. Exit code 0 means everything works.

### Hub settings

`sommelier serve` takes these flags. `start-hub.ps1` passes only `--listen` and `--data`, edit the script to change the rest.

| Flag | Default | Meaning |
|---|---|---|
| `--listen ADDR` | `127.0.0.1:7300` | Address to listen on. Keep it on the loopback address behind the tunnel. |
| `--data DIR` | `SOMMELIER_DATA`, else `%LOCALAPPDATA%\sommelier` on Windows, `$XDG_DATA_HOME/sommelier` or `~/.local/share/sommelier` on Linux | Data folder. `sommelier token` uses the same default. |
| `--rate N` | `2` | Requests per second per member name. |
| `--burst N` | `30` | Requests a member may send at once before the rate applies. |
| `--claim-ttl MINUTES` | `60` | A claim whose holder sent nothing (no message, no heartbeat) for this long is released by the hub. |
| `--max-body BYTES` | `4096` | Largest message body. |
| `--quiet` | off | Do not print connects and disconnects. |

## Tokens

Every person (or every agent that should appear under its own name) gets one token. A name is 1 to 32 characters from `a` to `z`, `0` to `9`, `_`, `.` and `-`, in any case (stored in lower case, so `Alice` becomes `alice`), and never `hub`. The name is what everyone sees in the `from` field.

The token commands edit `tokens.json` directly, so they work whether the hub runs or not. They use the same default data folder as `sommelier serve` (and print the token file they use on stderr), so `--data` is only needed for a folder other than the default:

```powershell
$data = "$env:LOCALAPPDATA\sommelier"
.\target\release\sommelier.exe token add alice --data $data
.\target\release\sommelier.exe token add owner --role orchestrator --data $data
.\target\release\sommelier.exe token add dashboard --role observer --data $data
.\target\release\sommelier.exe token list --data $data
.\target\release\sommelier.exe token revoke alice --data $data
```

* `token add` prints the token (`sb_` and 48 hex digits) on stdout and a note on stderr. The default role is `member`. Hand the token over privately (a direct message, never an issue comment or the bus itself).
* `token list` shows name, role and creation time. The file holds only salted hashes, so a lost token cannot be shown again: `sommelier token rotate NAME` issues a new one with the same role and the old one stops working within seconds.
* `token revoke` removes the entry. A running hub rereads `tokens.json` every 2 seconds and closes every open connection of that token within about 2 more seconds. The client sees `token revoked` and stops.
* There is no rotate command. To replace a token: `revoke`, then `add` with the same name. `add` refuses a name that already has a token.

### Roles

| Role | Read | Send | Extra rights |
|---|---|---|---|
| `orchestrator` | everything | every kind | The only role that may send `TASK`. May `RELEASE` a claim held by someone else. |
| `member` | everything | every kind except `TASK` | Releases only its own claims. |
| `observer` | everything | nothing | Read-only role for monitors and dashboards. All clients authenticate via `Authorization: Bearer <token>` (or `POST /api/session` `HttpOnly` cookie in browsers); `?token=` is refused for all roles. |

Give the owner's orchestrator session an `orchestrator` token. Give every contributor a `member` token. Use `observer` tokens for dashboards and for agents that should only watch.

Every role can read the whole log and can ask `sommelier who` and `sommelier claims`. There are no private messages and no per topic permissions.

## What the owner can see

* Everything on the bus, like every member: `sommelier log`, `sommelier tail`, and the raw `bus.jsonl`.
* Every token in `tokens.json`. The owner can technically act as any member. Say so openly to the team.
* Who is connected right now and in which mode: `sommelier who` (`listening` for a tail, `request` for a one shot command).
* Connects and disconnects of listeners on the hub console.
* Cloudflare sees the traffic in clear (TLS ends there), while tokens stay out of URLs (`?token=` is refused with HTTP 401 for every role). CLI/service clients send tokens in the `Authorization: Bearer` header, and the web dashboard exchanges a token once via `POST /api/session` for an `HttpOnly; SameSite=Strict` cookie.
* The hub stores no IP addresses. The client address it sees is always `127.0.0.1` (the tunnel).

## Reading the log

```powershell
$env:SOMMELIER_URL = "ws://127.0.0.1:7300"   # or the public address
$env:SOMMELIER_TOKEN = "<your token>"
sommelier log --last 50
sommelier log --since 1200 --limit 200 --kind READY,LANDED
sommelier log --from alice --last 20
sommelier claims
sommelier who
```

`log` returns at most 1000 messages per call (100 by default). For everything, read `bus.jsonl` directly.

## When a token leaks

1. Revoke it at once: `sommelier token revoke NAME --data $data`. The connections drop within seconds.
2. Issue a new one with `token add NAME` and hand it over privately.
3. Look at what the name sent since the leak: `sommelier log --from NAME --last 100`. Tell the team on the bus (CHAT) and on the tracking issue if anything was sent that the member did not write.
4. If `tokens.json` itself leaked, revoke and reissue every token.
5. If a message carried a secret: the bus cannot delete messages. Stop the hub, replace that line's `body` in `bus.jsonl` with `[removed]` (keep the line and its `id`, the ids must stay dense), start the hub again. Everyone who was connected already has a copy, so rotate the secret itself as well.
6. If the tunnel credentials (`%USERPROFILE%\.cloudflared\<tunnel id>.json` or `cert.pem`) leaked: `cloudflared tunnel delete sommelier`, `cloudflared tunnel create sommelier`, `cloudflared tunnel route dns --overwrite-dns sommelier hub.example.com`, update the tunnel id in `config.yml`, and start the hub again. Clients need no change.

## Moving the hub to another machine

The address stays the same, so members change nothing.

1. Stop the hub on the old machine (`scripts\stop-hub.ps1`).
2. Copy the data folder (`bus.jsonl` and `tokens.json`) to the new machine by a private path, for example a USB stick or `scp`. Never through the repository.
3. Install Rust (the toolchain in `rust-toolchain.toml` installs itself through rustup) and cloudflared, build sommelier.
4. Run the tunnel there, either way:
   * Same tunnel: copy `config.yml` and the tunnel credentials file (`<tunnel id>.json`) privately to `~/.cloudflared/` on the new machine and fix the `credentials-file` path in `config.yml`. Never run the same tunnel on both machines at once.
   * New tunnel: `cloudflared tunnel login`, `cloudflared tunnel create sommelier2`, `cloudflared tunnel route dns --overwrite-dns sommelier2 hub.example.com`, and an ingress rule `hub.example.com` to `http://127.0.0.1:7300` in `config.yml`.
5. Start the hub with `--data` pointing at the copied folder. On Windows use `scripts\start-hub.ps1 -Data <folder>` (and `-Tunnel sommelier2` for a new tunnel). On Linux:

   ```sh
   sommelier serve --data ~/sommelier-data &
   cloudflared tunnel run sommelier
   ```

6. Run `scripts/check.ps1` (or `sommelier whoami` and a CHAT) through the public address.

The hub rebuilds the message ids and the claim table from `bus.jsonl` on start. Claims restored this way get a fresh timeout from the moment of the start.
