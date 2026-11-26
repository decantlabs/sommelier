---
version: 1
slug: "ui"
primary_target: "ui"
related_targets: []
---

# Surface Brief: Sommelier Dashboard (`bus/ui`)

- **Scope:** `bus/ui` (`index.html`, `style.css`, `app.js`, `views/*.js`, `assets/favicon.svg`)
- **Visitor mode:** `operate`
- **Primary user & job:** Human operators and orchestrators monitoring live Decant AI agent swarms on the Sommelier message bus—spotting blocked/suspect workers, draining quota pools, regressions, and active claims/runs at a glance.
- **Constraints:** Zero build step, zero external CDNs/fonts, strict CSP (`style-src 'self'`, zero inline `style="..."` or `<style>`), read-only, light & dark modes, responsive desktop/tablet/phone, preserve `tests/quota_live.rs` contracts in `views/pools.js`, keep `bus/DESIGN.md` untouched.

## Direction contract

- **THESIS:** Operational swarm observatory modeled on Swiss cartographic survey sheets—every participant as a calibrated station with geometric transit-line worker tracks, SVG hatched quota gauges, and immediate anomaly triage—refusing generic dark-slate neon SaaS cards and terminal costumes.
- **OWN-WORLD:** Deep survey-indigo shell bar (`#162235` / `oklch(0.24 0.045 252)`), cool survey-paper ground (`oklch(0.972 0.005 245)` light; nocturnal chart slate `oklch(0.165 0.016 248)` dark), crisp elevation cards (`oklch(0.994 0.002 245)` light; `oklch(0.205 0.018 248)` dark), contour hairlines, survey-cobalt primary selection (`oklch(0.49 0.17 256)`), topographic emerald running, signal vermilion blocked/regression, calibrated amber suspect/quiet/drain. System sans + tabular monospace; zero inline styles.
- **STORY:** Operator opens the dashboard, immediately spots blocked/suspect workers, draining pools, or confirmed regressions in the top pulse + triage strip, scans participant cards and worker transit lines, switches Control Plane tabs (Tasks, Pools, Landing, Runs, Branches, Metrics, Claims) without losing sight of the docked live feed, and clicks topics/`#id`s to trace events.
- **FIRST VIEWPORT:** Committed survey-indigo sticky top bar (wordmark + transit glyph, live tabular pulse counters, connection pill, theme toggle, forget token); conditional anomaly triage strip right below; crisp white/slate workspace tab bar with live counts & alert dots; 2-column cockpit grid below: left primary workspace (3-column subgrid participant station cards + Control Plane panels + Claims/Runs), right sticky docked live bus feed with kind-filter pills and search.
- **FORM:** Cartographic Survey & Topographic Atlas (grounded candidate #6 of 7; `seed:814861fbc57c5434`; raised by declined challengers: distinct per-participant hue coding, fixed-scale comparative card/meter registration, color+shape dual encoding, crisp stamped status badges).
- **FINISH:** unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
