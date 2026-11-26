---
name: sommelier-ui
description: Operational swarm observatory modeled on Swiss cartographic survey sheets.
colors:
  survey-indigo: "oklch(0.235 0.045 252)"
  survey-paper: "oklch(0.97 0.005 245)"
  elevation-surface: "oklch(0.995 0.002 245)"
  sunk-tint: "oklch(0.942 0.008 245)"
  contour-rule: "oklch(0.865 0.012 245)"
  ink-primary: "oklch(0.21 0.025 252)"
  ink-secondary: "oklch(0.39 0.022 250)"
  ink-muted: "oklch(0.49 0.018 248)"
  survey-cobalt: "oklch(0.48 0.17 256)"
  survey-cobalt-solid: "oklch(0.44 0.18 256)"
  topographic-emerald: "oklch(0.51 0.155 154)"
  signal-vermilion: "oklch(0.53 0.21 26)"
  calibrated-amber: "oklch(0.57 0.14 74)"
typography:
  headline:
    fontFamily: "system-ui, -apple-system, 'Segoe UI Variable Text', 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "1.125rem"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "-0.015em"
  title:
    fontFamily: "system-ui, -apple-system, 'Segoe UI Variable Text', 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "0.9375rem"
    fontWeight: 700
    lineHeight: 1.4
    letterSpacing: "-0.01em"
  body:
    fontFamily: "system-ui, -apple-system, 'Segoe UI Variable Text', 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "ui-monospace, 'Cascadia Mono', 'Cascadia Code', 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace"
    fontSize: "0.75rem"
    fontWeight: 600
    lineHeight: 1.35
rounded:
  sm: "5px"
  md: "7px"
  lg: "8px"
  card: "12px"
  pill: "999px"
spacing:
  xs: "0.35rem"
  sm: "0.75rem"
  md: "1.15rem"
  lg: "1.5rem"
  xl: "2rem"
components:
  button-primary:
    backgroundColor: "{colors.survey-cobalt-solid}"
    textColor: "{colors.elevation-surface}"
    rounded: "{rounded.md}"
    padding: "0 1.15rem"
  button-shell:
    backgroundColor: "transparent"
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.md}"
    padding: "0 0.6rem"
  card-station:
    backgroundColor: "{colors.elevation-surface}"
    textColor: "{colors.ink-primary}"
    rounded: "{rounded.lg}"
    padding: "1rem 1.15rem"
---

# Design System: Sommelier Dashboard

## Overview

**Creative North Star: "Cartographic Survey & Topographic Atlas"**

The Sommelier web dashboard (`bus/ui/`) is an operational swarm observatory modeled on Swiss cartographic survey sheets and transit control schedules. Every swarm participant renders as a calibrated survey station with a vertical transit-line worker track, paired with precision SVG hatched quota gauges, an immediate top anomaly triage strip, and a docked live bus feed.

The system runs zero-build vanilla HTML, CSS, and ES modules under a strict Content Security Policy (`style-src 'self'`, `script-src 'self'`, `font-src 'self'`). Every visual state is expressed through CSS custom properties, classes, `data-*` attributes, and SVG geometry attributes—never inline `style="..."` attributes or external CDN assets.

**Key Characteristics:**
- Committed deep survey-indigo header shell (`oklch(0.235 0.045 252)`) anchoring live tabular telemetry counters and connection state.
- Dual-polarity OKLCH token architecture: cool survey-paper light ground (`oklch(0.97 0.005 245)`) and nocturnal chart slate dark ground (`oklch(0.165 0.016 248)`), switchable via system preference or manual `data-theme` override.
- Dual-encoded worker and state semantics combining hue with distinct geometric node silhouettes.

## Colors

The palette uses perceptually uniform OKLCH tokens so status hues retain consistent visual weight across light and dark modes.

### Primary
- **Survey Indigo** (`oklch(0.235 0.045 252)`): Committed sticky shell bar background anchoring navigation, live pulse metrics, and connection status.
- **Survey Cobalt** (`oklch(0.48 0.17 256)`): Active workspace tab indicator, primary focus/selection accents, and SVG quota meter used fill (`oklch(0.44 0.18 256)` solid for primary buttons).

### Secondary
- **Topographic Emerald** (`oklch(0.51 0.155 154)`): Live WebSocket indicator, online presence, running workers, and passing verification/run states.
- **Signal Vermilion** (`oklch(0.53 0.21 26)`): Blocked/suspect/starved workers, confirmed metric regressions, refused connections, and quota threshold lines.
- **Calibrated Amber** (`oklch(0.57 0.14 74)`): Quiet/stale workers, draining or throttled quota pools, and reconnecting states.

### Neutral
- **Survey Paper** (`oklch(0.97 0.005 245)` light / `oklch(0.165 0.016 248)` dark): Primary page canvas.
- **Elevation Surface** (`oklch(0.995 0.002 245)` light / `oklch(0.205 0.018 248)` dark): Station cards, Control Plane panels, and docked feed container.
- **Sunk Tint** (`oklch(0.942 0.008 245)` light / `oklch(0.145 0.014 248)` dark): Recessed pool rows, metadata chips, and code blocks.
- **Contour Rule** (`oklch(0.865 0.012 245)` light / `oklch(0.315 0.022 248)` dark): 1px structural borders and dividers.

### Named Rules
**The Zero-Inline-Style Rule.** All color, layout, and state variations must use CSS classes, `data-*` attributes, or SVG geometry attributes; inline `style="..."` attributes are forbidden by the hub CSP.
**The Dual-Encoding Rule.** State color never works alone: running, blocked (rotated diamond), idle (hollow ring), quiet (amber ring), and done (muted dot) each carry distinct geometry and explicit text labels.

## Typography

**Display & Body Font:** System UI neo-grotesque stack (`system-ui, -apple-system, "Segoe UI Variable Text", "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`)
**Label & Data Font:** System monospace stack (`ui-monospace, "Cascadia Mono", "Cascadia Code", "SF Mono", Menlo, Consolas, "Liberation Mono", monospace`)

**Character:** Unembellished operational legibility with `font-variant-numeric: tabular-nums` enforced globally on `body` so live counters, timestamps, and percentages align vertically without jitter.

### Hierarchy
- **Headline** (`700`, `1.125rem`, `1.2`, `-0.015em`): Participant station names and gate title.
- **Title** (`700`, `0.9375rem`, `1.4`, `-0.01em`): Panel headings (Tasks, Quota pools, Landing, Runs, Branches, Metrics, Claims, Feed).
- **Body** (`400`, `0.875rem`, `1.45`): Message bodies, agent status descriptions, and control labels.
- **Label / Mono** (`600`, `0.75rem`, `1.35`): Topic slugs, commit SHAs, branch chips, timestamps, and uppercase micro-labels (`0.6875rem`, `0.05em–0.06em` tracking).

## Layout

- **Cockpit Grid (`>1180px`):** `max-width: 1680px` centered container with a sticky shell bar, full-width workspace tab strip, conditional anomaly triage strip, and a 2-column body (`minmax(0, 1fr)` left workspace + `minmax(22rem, 27.5rem)` sticky docked right Feed).
- **Participant Subgrid:** `#people` uses `repeat(auto-fill, minmax(min(100%, 17.5rem), 1fr))` with 5-row CSS `subgrid` so participant headers, machine rigs, headlines, activity rows, and worker tracks align across adjacent cards.
- **Breakpoints:** Medium (`<=1180px`) stacks Feed below Workspace; Compact header (`<=900px`) wraps pulse counters below the brand mark; Phone (`<=640px`) enables the sticky mobile section jump bar and horizontal kind-filter scroll.

## Elevation & Depth

Cards and panels sit one subtle step above the survey-paper ground using crisp `1px solid var(--rule)` contour borders paired with soft offset ambient shadows (`0 1px 2px oklch(0.24 0.03 252 / 0.06), 0 4px 12px oklch(0.24 0.03 252 / 0.05)` in light mode).

## Shapes

- **Cards & Panels:** `8px` radius (`12px` on the standalone Token Gate card).
- **Controls & Inputs:** `7px` radius (`6px` on workspace tabs and compact agent rows; `5px` on metadata chips).
- **Status Pills & Presence Dots:** `999px` full pill / `50%` circle (with `3px` rotated `45deg` diamond for blocked/suspect worker nodes).

## Components

### Buttons
- **Shape:** `7px` radius (`min-height: 2rem`).
- **Primary:** Survey Cobalt solid fill (`oklch(0.44 0.18 256)`) with near-white ink (`oklch(0.99 0.002 250)`), darkening on hover (`oklch(0.38 0.18 256)`).
- **Shell Action:** Transparent button inside the Survey Indigo header with `1px solid var(--shell-border)` and muted shell ink, brightening on hover.

### Workspace Tabs
- Horizontal `tablist` inside `.ws-tabs-wrap` with `6px` rounded pill tabs, live item count badges (`.tab-n`), and vermilion `.has-alert` indicator dots.

### Calibrated Quota Meter (`.pool-bar`)
- CSP-safe `<svg class="pool-meter-svg" viewBox="0 0 100 10" preserveAspectRatio="none">` rendering solid used fill, SVG `<pattern>` 45-degree diagonal hatch for committed quota, shaded reserve band, and a `vector-effect="non-scaling-stroke"` `2px` vermilion threshold line. Stale samples apply `.is-greyed` to shift active fills to neutral `--off`.

### Participant Station & Worker Transit Line
- Card header with arrival-ordered `.hue-0`..`.hue-7` dot, role pill, presence line, and collapsible `HELLO` machine spec `<dl>`.
- Vertical transit track (`.line::before`) connecting grouped worker rows (`Blocked`, `Suspect`, `Running`, `Starved`, `Idle`, `Quiet`, `Done`).

## Do's and Don'ts

### Do:
- **Do** keep all visual styling in `bus/ui/style.css` using classes, `data-*` attributes, and SVG attributes.
- **Do** preserve `bus/tests/quota_live.rs` DOM/export contracts (`renderPools`, `pool-bar-used`, `pool-bar-committed`, `pool-bar-line`, `pool-bar-reserve`, `waiting_tasks`, `is-greyed`).
- **Do** bump `?v=` query parameters in `bus/ui/index.html` whenever editing `style.css`, `app.js`, or `views/*.js`.

### Don't:
- **Don't** emit inline `style="..."` attributes or `<style>` tags in HTML or JS templates.
- **Don't** add external CDN fonts, stylesheets, or scripts.
- **Don't** overwrite `bus/DESIGN.md` (which documents the Rust bus backend).
