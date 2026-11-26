# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Human operators, maintainers, and orchestrators monitoring the Sommelier coordination bus for Decant (an Xbox-to-native PC compatibility layer) and its AI coding agent swarms (Claude Code, Codex, Gemini, Hermes) across desktop workstations and mobile devices.

## Product Purpose

Sommelier's web dashboard is a read-only live operational cockpit that answers at a glance who is running on each participant machine, what every worker is doing, which agents are blocked or suspect, how quota pools and control-plane queues stand, and what is happening on the message bus in real time.

## Positioning

A zero-build, single-port, observer-authenticated operational dashboard served directly from the Rust `sommelier` hub binary over a strict Content Security Policy—combining live participant/worker presence, control-plane telemetry (tasks, quota pools, landing queue, game runs, branches, regression metrics), and a filterable bus feed with no external dependencies.

## Operating Context

- Served from `bus/ui/` by the Rust hub (`bus/src/web.rs`) on loopback or behind a Cloudflare Tunnel (`wss://`).
- Read-only observer surface: authenticated via an observer token (`sommelier token add <name> --role observer`) stored in browser `localStorage` (`sommelier.token`), never in the URL bar or logs.
- Initial paint hydrates from `GET /api/state` (`Authorization: Bearer` header, `?token=` fallback) and streams live updates over WebSocket (`since=<hub.id + 1>` or `last=200`), polling `who` and `claims` every 20 seconds and refreshing the member roster every 60 seconds.
- Must work cleanly on desktop (>1180px), tablet (641–1180px), and phone (<=640px) in both light and dark modes.

## Capabilities and Constraints

- **Zero build step & zero external CDNs:** Plain HTML, CSS, and ES-module JavaScript only (`bus/ui/index.html`, `style.css`, `app.js`, `views/*.js`, `assets/favicon.svg`).
- **Strict Content Security Policy (`bus/src/web.rs`):** `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self' wss://<host> ws://<host>; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`. Inline `style="..."` attributes and `<style>` tags are forbidden; variable graphics must use CSS classes, `data-*` attributes, or SVG geometry attributes.
- **Read-only:** Nothing can be mutated or sent to the bus from the UI.
- **Test contracts:** Preserve `bus/tests/quota_live.rs` DOM/export contracts in `bus/ui/views/pools.js` (`export function renderPools`, `pool-bar-used`, `pool-bar-committed`, `pool-bar-line`, `pool-bar-reserve`, `waiting_tasks`, `is-greyed`) and `bus/tests/bus.rs` UI serving expectations.
- **Cache busting:** Update `?v=` query parameters on stylesheet and script tags in `bus/ui/index.html` when shipping changes because Cloudflare caches static assets for 4 hours.
- **Confirmed scope:** Complete visual redesign of the dashboard plus full visual and structural integration of all six Control Plane views (`tasks`, `pools`, `landing`, `runs`, `branches`, `metrics`) in an operative cockpit layout (participant/agent status first, blocked/suspect/running surfaced immediately, cleanly organized Control Plane sections, and docked live feed).

## Brand Commitments

- Name: `sommelier` (lowercase wordmark).
- Tone: Direct, factual, unembellished operational clarity. No marketing fluff, no invented telemetry, no wine-sommelier kitsch.

## Evidence on Hand

- Live dashboard source: `bus/ui/index.html`, `bus/ui/style.css`, `bus/ui/app.js`, `bus/ui/views/{index,tasks,pools,landing,runs,branches,metrics}.js`, `bus/ui/assets/favicon.svg`.
- Documentation: `bus/docs/DASHBOARD.md`, `README.md`, `bus/DESIGN.md` (Rust bus architecture—do not overwrite).
- Automated checks: `cargo test` in `bus/` (including `tests/quota_live.rs` and `tests/bus.rs`).

## Product Principles

- **Operational clarity over decoration:** Blocked, suspect, starved, draining, or regressed states must be unmistakable within one second of opening the page.
- **Strict truthfulness:** Never fabricate numbers, claims, or control actions; reflect `/api/state` and the WebSocket stream faithfully, including stale-frame warnings and empty states.
- **Zero-friction resilience:** Must run reliably over high-latency tunnels, reconnect cleanly, and preserve filters and view state without build tooling or external assets.

## Accessibility & Inclusion

- Semantic landmarks, keyboard-accessible tabs/controls/agent rows, `aria-live` status updates, `prefers-reduced-motion` support, and state indicators that never rely on color alone (distinct node shapes, labels, and contrast in both light and dark modes).
