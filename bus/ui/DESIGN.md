---
name: sommelier-ui
description: Software Factory Control Center for the Sommelier coordination bus.
colors:
  obsidian-ground: "#090a0d"
  graphite-rail: "#0d0f14"
  graphite-surface: "#12151c"
  graphite-elevated: "#171b24"
  contour-rule: "#232834"
  ink-primary: "#eef2f7"
  ink-secondary: "#a9b4c6"
  ink-muted: "#7c879b"
  factory-cobalt: "#638cff"
  factory-cobalt-solid: "#3b6cf6"
  signal-emerald: "#34d399"
  signal-rose: "#fb7185"
  signal-amber: "#fbbf24"
typography:
  headline:
    fontFamily: "'Geist', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif"
    fontSize: "1.0625rem"
    fontWeight: 650
    lineHeight: 1.2
    letterSpacing: "-0.015em"
  title:
    fontFamily: "'Geist', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif"
    fontSize: "0.9375rem"
    fontWeight: 650
    lineHeight: 1.4
    letterSpacing: "-0.01em"
  body:
    fontFamily: "'Geist', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.48
  label:
    fontFamily: "'JetBrains Mono', ui-monospace, 'Cascadia Mono', Menlo, Consolas, monospace"
    fontSize: "0.75rem"
    fontWeight: 600
    lineHeight: 1.35
rounded:
  sm: "5px"
  md: "8px"
  lg: "10px"
  card: "14px"
  pill: "999px"
spacing:
  xs: "0.35rem"
  sm: "0.75rem"
  md: "1rem"
  lg: "1.25rem"
  xl: "1.4rem"
components:
  button-primary:
    backgroundColor: "{colors.factory-cobalt-solid}"
    textColor: "#ffffff"
    rounded: "{rounded.md}"
    padding: "0 1.15rem"
  card-panel:
    backgroundColor: "{colors.graphite-surface}"
    textColor: "{colors.ink-primary}"
    rounded: "{rounded.lg}"
    padding: "1rem 1.15rem"
---

# Design System: Sommelier Software Factory

## Overview

**Creative North Star: "Software Factory Control Center"**

The Sommelier web dashboard (`bus/ui/`) is a 3-pane operational control center for AI coding agent swarms and Decant's build/verification pipeline. A fixed left navigation rail (`232px`) separates Factory views (**Overview**, **Feature Map**, **Fleet**) from Control Plane views (**Orders**, **Quota Pools**, **Delivery**, **Quality**, **Claims & Runs**), paired with a central workspace and a collapsible right-hand **Live Feed** (`368px`).

Self-hosted variable WOFF2 fonts (`Geist` for UI/display, `JetBrains Mono` for feature IDs, SHAs, branches, and telemetry) are served directly from `bus/ui/assets/fonts/` under the hub's strict `font-src 'self'` and `style-src 'self'` Content Security Policy—with zero inline `style="..."` attributes and zero external CDN requests.

## Colors

- **Obsidian Ground** (`#090a0d` dark / `#f5f7fa` light): Primary workspace canvas.
- **Graphite Rail** (`#0d0f14` dark / `#edf0f5` light): Left navigation rail and right Live Feed dock.
- **Graphite Surface** (`#12151c` dark / `#ffffff` light): Elevated KPI tiles, fleet cards, feature graph canvas, and control-plane panels.
- **Factory Cobalt** (`#638cff` / `#3b6cf6` solid): Active selections, primary buttons, and quota meter used fills.
- **Signal Emerald / Rose / Amber** (`#34d399` / `#fb7185` / `#fbbf24` dark): Proven features & running workers, blocked/failing/regression states, and suspect/draining/stale states.

## Typography

- **UI & Display:** Self-hosted variable `Geist` (`400..700`) with `font-variant-numeric: tabular-nums`.
- **Code & Telemetry:** Self-hosted variable `JetBrains Mono` (`400..600`).

## Do's and Don'ts

- **Do** keep all styling in `bus/ui/style.css` using classes, `data-*` attributes, and SVG attributes.
- **Do** preserve `bus/tests/quota_live.rs` DOM/export contracts in `bus/ui/views/pools.js`.
- **Don't** emit inline `style="..."` attributes, `<style>` tags, or external CDN URLs.
- **Don't** overwrite `bus/DESIGN.md` (Rust backend architecture notes).
