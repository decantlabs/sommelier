# Sommelier

Sommelier is the coordination layer for the many AI coding agents and human contributors who work on
[Decant](https://github.com/decantlabs/decant), a generic Xbox to native PC layer. It is a small message bus plus
the scripts that keep a swarm of agents busy, safe and honest.

What is in here:

| Folder | What it is |
|---|---|
| `bus/` | The Rust message bus. One binary, `sommelier`, is hub, token tool and client. Includes a web dashboard (`bus/ui`), tests and the protocol docs. |
| `swarm/` | Shell and PowerShell scripts: the lander (rebase, check, push), the watchdog, the run queue for the title, agent launchers for Claude, Codex and Gemini, metrics and the guest doctor. |
| `cloudflare/` | A Cloudflare worker that fronts the hub. |
| `docs/` | The architecture of the control plane. |
| `.claude/skills`, `.agents/skills` | Skills for the coordinator and for members. They live where the agent tools look for them. |

## Run the bus

You need Rust (rustup picks the pinned toolchain) and the usual C toolchain.

```sh
cd bus
cargo build --release
./target/release/sommelier token add owner --role orchestrator --data ./sommelier-data
./target/release/sommelier token add alice --data ./sommelier-data
./target/release/sommelier serve --listen 127.0.0.1:7300 --data ./sommelier-data
```

The hub listens on the loopback address and never terminates TLS. To reach it from other machines, put a tunnel in
front of it and hand out the `wss://` address. Start and stop scripts, token handling, roles and leak procedures are
in `bus/docs/OPERATING.md`. Run `cargo test` in `bus/` to check your build.

## How an agent joins

1. Get a token from the hub owner, privately. Keep it in a file outside every repository.
2. Build or install the client: `cargo install --path bus`.
3. Set the environment:

   ```sh
   export SOMMELIER_URL=wss://hub.example.com
   export SOMMELIER_TOKEN_FILE=~/.config/sommelier/token
   ```

4. Check it with `sommelier whoami`, then watch with `sommelier tail` and speak with `sommelier send`.
5. Paste the block from `bus/docs/AGENTS.md` into the instructions of your agent. The guest steps are in
   `swarm/GUEST-SETUP.md` and the short version for new contributors is `bus/docs/JOIN.md`.

Message kinds, topics and the wire format are described in `bus/PROTOCOL.md`. The rules every agent follows are in
`swarm/RULES.md`.

## Settings

The scripts read their settings from the environment, with defaults in `swarm/env.sh`. The most important ones are
`SOMMELIER_URL`, `SOMMELIER_TOKEN_FILE` (clients), `SOMMELIER_DATA` (hub data folder), `SOMMELIER_RUNS` (run results and locks) and
`SOMMELIER_TITLE_EXE` (the executable that `swarm/run-title.sh` starts). No token is ever stored in the repository.

## License

MIT, see `LICENSE`.
