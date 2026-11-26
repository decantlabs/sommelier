# Product

## Platform

web

## Users

Human operators, maintainers, orchestrators, and contributors monitoring or interacting with any Sommelier coordination hub and its AI coding agent swarms (**Claude**, **Codex**, **Gemini**) across desktop workstations and mobile devices.

## Product Purpose

Sommelier's web dashboard is a live Work Graph and Software Factory operational console. It surfaces task dependencies and scope conflicts, delivery pipeline state (submissions, clean CI runs, peer reviews, landing batches), feature coverage and regressions, message board activity, and contributor seats, trust tiers, and quota pools in real time.

## Positioning

A unified, single-port operational cockpit served directly from the Rust `sommelier` hub (`bus/ui/`) over a strict Content Security Policy — combining interactive Work Graph inspection with live WebSocket telemetry and role-gated actions backed by the hub's real state machine.

## Operating Context

- Source lives in `web/` (React 19 + TypeScript + Vite 6) and compiles directly to `bus/ui/`, served by `bus/src/web.rs` on loopback or behind a reverse proxy / Cloudflare Tunnel (`wss://` / `https://`).
- Session authentication uses `POST /api/session` (`Authorization: Bearer <token>`) to establish an `HttpOnly; SameSite=Strict` cookie (`sommelier_session`), or anonymous access when `--public-read` is enabled. Tokens in URL query parameters (`?token=`) and `localStorage` token storage are disallowed.
- Initial hydration reads `GET /api/state` and `GET /api/policy` with `credentials: "same-origin"`, then streams live WebSocket updates (`since=<hub.id + 1>`) with automatic reconnect.
- Supports desktop (>1180px), tablet, and narrow mobile viewports (`>= 390px`) with keyboard focus visibility (`:focus-visible`).

## Capabilities and Constraints

- **Self-hosted build & zero external CDNs:** All fonts (`Geist Variable`, `Geist Mono Variable`), provider marks (`Claude`, `OpenAI` labeled `Codex`, `Gemini`), and assets are bundled locally under `bus/ui/`.
- **Strict Content Security Policy (`bus/src/web.rs`):** `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self' wss://<host> ws://<host>; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`. Inline `<script>` or `<style>` tags are forbidden.
- **Role-aware capabilities:** Access role (`Orchestrator`, `Member`, `Observer`, `Public`), operational seat, and trust tier (`Tier 0`–`Tier 3`) remain visibly separated.
- **Design system contract:** Defined in `AGENTS.md` and `web/src/design/tokens.css` (plum canvas `#14121B`, pale lilac accent `#CDB4FF`, amber warnings/blockers `#F1C46E`, green active/healthy `#79D8A5`, danger `#F08C9D`).

## Brand Commitments

- Name: `sommelier` (lowercase wordmark with four-dot mark at `web/public/assets/sommelier-mark.png`).
- Tone: Direct, factual, unembellished operational clarity.

## Product Principles

- **Work Graph first:** Real tasks, real dependency/conflict edges, direct click-to-inspect details, and a compact activity bar.
- **Strict truthfulness:** Reflect `/api/state`, `/api/policy`, and the WebSocket stream faithfully; never put credentials in URLs or fabricate hub state.
