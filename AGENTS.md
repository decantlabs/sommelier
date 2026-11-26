# Sommelier Agent & UI Design Guidelines

## 1. Design System Source of Truth

All frontend UI lives in `web/src/` and builds into `bus/ui/` (`cd web && npm run build`).
Every new screen, panel, or widget **must** derive its appearance from the shared tokens and primitives instead of inventing new colors, fonts, or custom controls:

- **Tokens**: `web/src/design/tokens.css` (and `web/src/design/tokens.ts` for `<canvas>` edge/port colors)
- **Base styles & focus rings**: `web/src/design/base.css`
- **Shared primitives**: `web/src/components/` (`Button`, `StatusLabel`, `ProviderMark`, `ProviderLabel`, `Avatar`, `SegmentedControl`, `FilterBar`, `FilterField`, `FilterToggle`, `InspectorPanel`, `DefinitionList`, `CodeChip`, `TaskNode`, `MeterBar`)

## 2. Binding Visual Rules

- **Canvas & Chrome**: Dark, slightly plum canvas (`--color-bg: #14121b`, `--color-canvas: #13111a` with subtle `--color-canvas-dot: #332b40` dot grid). Keep the top navigation bar compact and leave generous room for the primary graph canvas.
- **Typography**: Use `Geist Variable` (`--font-sans`) for all UI copy and `Geist Mono Variable` (`--font-mono`) for IDs, paths, commit SHAs, timestamps, and machine/pool keys. Both are self-hosted via `@fontsource-variable/*`.
- **Brand Mark**: Always use the violet four-dot mark at `web/public/assets/sommelier-mark.png`.
- **Semantic Color Roles**:
  - **Pale Lilac (`#CDB4FF` / `--color-accent`)**: Actions, focus rings (`:focus-visible`), active tabs, selected nodes, and interactive links.
  - **Amber (`#F1C46E` / `--color-warning`)**: Warnings, review/audit waits, and dispatch blockers. Scope conflicts must additionally use dashed amber edges.
  - **Green (`#79D8A5` / `--color-good`)**: Running, closed, passed, or healthy states paired with an explicit status label.
  - **Danger (`#F08C9D` / `--color-danger`)**: Hard failures (`gate_failed`, `verify_failed`, `review_failed`, `Stuck`, `REGRESSION`, `KEY_REVOKED`).
- **Provider Marks**: Render Claude, OpenAI, and Gemini using the local SVGs under `web/public/assets/providers/` (`ProviderMark`). **The OpenAI icon must always be labeled `Codex`.** Provider brand colors belong strictly to the icons; UI chrome stays lilac.

## 3. Domain & Layout Conventions

- **Work Graph First**: `WorkGraphView` is the primary project view. Nodes represent real tasks; edges represent concrete dependencies or scope conflicts; clicking a node opens its details directly in `InspectorPanel`; keep the bottom `Activity` bar compact and collapsed by default.
- **Master-Detail Consistency**: Queues, logs, matrices, and rosters (`Pipeline`, `Testing`, `Message board`, `Contributors`) must reuse `FilterBar`, `SegmentedControl`, `StatusLabel`, and a docked `InspectorPanel` with `DefinitionList` and `CodeChip`.
- **Separate Orthogonal Concepts**:
  - Keep **Access role** (`Orchestrator`, `Member`, `Observer`, `Hub`), **Operational seat** (`Coordinator`, `Worker`, `Guest lead`, `Assigned auditor`, `Brain`, `Lander`, `Run keeper`), and **Trust tier** (`Tier 0`–`Tier 3`) strictly separate.
  - Keep **Task state** (`Draft`, `Ready`, `Offered`, `Leased`, `Running`, `Delivered`, `Landing`, `Closed`, `Stuck`, `Cancelled`) separate from **Dispatch blockers** (e.g. `Ready` + `Waiting on active scope`).

## 4. Rust Hub & CSP Constraints

- `bus/src/web.rs` serves `bus/ui/` with a strict CSP (`script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'`). Never add inline `<script>` or `<style>` tags to `web/index.html`, and never depend on external CDNs.
- Never place any token (observer, member, orchestrator, or key) into URL query parameters (`?token=` is refused with HTTP 401 by the hub for all roles). Browser authentication exchanges an `Authorization: Bearer <token>` header via `POST /api/session` for an `HttpOnly; SameSite=Strict` session cookie.
- Keep demo copy generic (no personal or game-specific names).
