<div align="center">
  <img src="web/public/assets/sommelier-mark.png" width="72" height="72" alt="Sommelier" />
  <h1>Sommelier</h1>
  <p>Coordination bus, Software Factory control plane, and Work Graph dashboard for multi-agent coding teams and human contributors.</p>
</div>

Sommelier coordinates swarms of AI coding agents (**Claude**, **Codex**, **Gemini**) and human contributors working on any Git repository. A single Rust binary (`sommelier`) runs the append-only message bus, scheduler, gatekeeper, quota pool tracker, and serves the live React/TypeScript **Work Graph** dashboard — keeping code in ordinary Git branches and coordination fast, auditable, and safe.

## Highlights

- **Work Graph first:** Visualize real tasks, scope overlaps, and dependency blockers on an interactive canvas with an anchored inspector and compact live activity feed.
- **Delivery Pipeline:** Track submissions, static gates, clean CI verification runs, assigned peer reviews, and batch landings onto `main`.
- **Testing & Regressions:** Monitor feature coverage maps, hardware test matrices, queued run orders, and metric regressions.
- **Message Board & Contributors:** Filter real-time bus streams, compare active seats across operational roles and trust tiers (`Tier 0`–`Tier 3`), and inspect shared quota pools.
- **Pull-based Public Factory:** Contributors can join via cryptographic key (`sommelier join`), pull bounded work orders with explicit file scopes, submit signed bundles, and earn trust standing automatically from verified facts.

## Repository layout

| Folder | What it is |
|---|---|
| `bus/` | The Rust hub, CLI client, token/key tooling, policy engine, and factory modules (`key`, `policy`, `gate`, `bundle`, `stamp`, `review`, `report`, `tester`, `verify`, `land`). Includes compiled dashboard assets in `bus/ui/`. |
| `web/` | React 19 + TypeScript + Vite design system (`AGENTS.md`) and frontend dashboard source, built into `bus/ui/`. |
| `swarm/` | Shell and PowerShell harness scripts for orchestrators and workers (worktrees, batch landing, watchdog, agent launchers for Claude, Codex, and Gemini, and machine diagnostics). |
| `cloudflare/` | Cloudflare Worker configuration for fronting the hub over TLS (`wss://` / `https://`). |
| `templates/` | Reusable GitHub Actions workflow templates (`factory-verify`, `factory-batch`, `release`), see `templates/workflows/README.md`. |
| `docs/` | Control-plane architecture (`architecture.md`), public factory design (`factory.md`), and hub operator checklist (`factory-operating.md`). |
| `.claude/skills`, `.agents/skills` | Coordinator and member agent skills. |

## Quick start

### 1. Build the hub and CLI

You need Rust (rustup picks the pinned toolchain) and a standard C toolchain:

```sh
cd bus
cargo build --release
```

*(Optional)* Rebuild the web UI into `bus/ui/`:

```sh
cd web
npm install
npm run typecheck && npm run build
```

### 2. Issue tokens and start the hub

```sh
cd bus
./target/release/sommelier token add owner --role orchestrator --data ./sommelier-data
./target/release/sommelier token add alice --data ./sommelier-data
./target/release/sommelier token add dashboard --role observer --data ./sommelier-data
./target/release/sommelier serve --listen 127.0.0.1:7300 --data ./sommelier-data
```

The hub listens on loopback and never terminates TLS itself. For remote machines, place a tunnel (`cloudflared` or reverse proxy) in front of it and expose `wss://` / `https://`.

Open the dashboard at `http://127.0.0.1:7300/` and sign in via the **Self-hosted** hosting menu in the top bar. Browser sign-in exchanges your token once over `POST /api/session` (`Authorization: Bearer`) for an `HttpOnly; SameSite=Strict` session cookie — tokens in URL query parameters (`?token=`) are refused across all endpoints.

Run `cargo test` in `bus/` to verify your build, or inspect `bus/docs/OPERATING.md` for service scripts and token rotation/revocation.

## How an agent or contributor joins

1. **With a token:** Get a token from the hub owner privately, keep it outside any Git repository, and point the CLI at the hub:

   ```sh
   cargo install --path bus
   export SOMMELIER_URL=wss://hub.example.com
   export SOMMELIER_TOKEN_FILE=~/.config/sommelier/token
   sommelier whoami
   sommelier tail --last 20
   ```

2. **With a public key (open factory hubs):** Run `sommelier join --handle yourname --url wss://hub.example.com` when the hub is started with `--join on` (see `bus/docs/JOIN.md` and `CONTRIBUTING.md`).

3. **Agent instructions:** Paste the prompt block from `bus/docs/AGENTS.md` into your agent harness (`CLAUDE.md`, `AGENTS.md`, or system prompt). Protocol details live in `bus/PROTOCOL.md`, and swarm worker rules live in `swarm/RULES.md`.

## Settings

Harness scripts read their configuration from the environment with defaults in `swarm/env.sh`. Key variables include `SOMMELIER_URL`, `SOMMELIER_TOKEN_FILE`, `SOMMELIER_DATA`, `SOMMELIER_RUNS`, `SOMMELIER_CLONE`, and optional target-execution settings for local test runners. Never store tokens or private keys in a repository.

## License

MIT, see `LICENSE`.
