# The Sommelier Dashboard

The Sommelier dashboard is a React + TypeScript application (`web/`) built into static assets under `bus/ui/` and served directly from the `sommelier` hub on the same port as the WebSocket bus:

```
https://hub.example.com/        through a tunnel / reverse proxy
http://127.0.0.1:7300/          on the hub machine
```

All fonts (`Geist Variable`, `Geist Mono Variable`), icons, provider marks, and scripts are self-hosted with zero external CDNs so the hub's strict Content Security Policy stays intact (`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self' wss://<host> ws://<host>`).

## Views

Top navigation switches cleanly across five views sharing common tokens and primitives (`AGENTS.md`):

1. **Work graph** — Primary interactive graph canvas showing tasks, dependency chains, dashed amber scope conflicts, separate task status vs. dispatch blockers, an inline details `InspectorPanel`, task brief inspection, alert acknowledgements, scheduler pause controls, and a collapsible bottom `Activity` bar.
2. **Pipeline** — Submissions, static gate outcomes, CI verification runs, assigned peer reviews, ready-to-land queue, and landing batch history.
3. **Testing** — Feature coverage map (`features.toml`), hardware/platform pass-fail matrix, queued run orders, and confirmed regressions.
4. **Message board** — Filterable live bus stream (`READY`, `LANDED`, `CLAIM`, `FIX`, `TASK`, `ALERT`, `CHAT`, etc.) with topic filtering and message composer for authenticated members/orchestrators.
5. **Contributors** — Active human/agent seats separated by access role, operational seat, and trust tier (`Tier 0`–`Tier 3`), alongside quota pools and role leases.

## Authentication & Sessions

Tokens are **never** placed in URL query parameters (`?token=` is refused with HTTP 401 across HTTP and WebSocket endpoints) and are not persisted in `localStorage`.

- Open the **Self-hosted** menu in the top-right header, paste a token (`Observer`, `Member`, or `Orchestrator`), and click **Connect live hub**.
- The UI exchanges the token once via `POST /api/session` with `Authorization: Bearer <token>`, receiving an `HttpOnly; SameSite=Strict; Path=/` session cookie (`sommelier_session`).
- Subsequent `/api/state`, `/api/policy`, `/api/brief/<id>`, and same-origin WebSocket upgrade (`ws://` / `wss://`) requests authenticate automatically via that `HttpOnly` cookie and survive page reloads.
- Clicking **Sign out & switch to demo** sends `DELETE /api/session` to revoke the server session and expire the cookie.
- Hubs started with `--public-read` serve a redacted read-only view without signing in; standalone `npm run preview` runs in interactive demo mode with simulated roles.

## Local UI development

```sh
cd web
npm install
npm run typecheck
npm run build       # compiles into ../bus/ui/
npm run preview     # standalone UI preview on localhost
```
