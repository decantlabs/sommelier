// Quota pools view (sections 6.4 and 7.7, step 9).
//
// Section 6.4:
// "How the UI shows a pool: one row per meter with a bar (used solid, committed hatched,
// the worker line and the reserve band marked), the state as a word and a colour,
// 'reaches the line in 2 h 10 min' or 'resets first, in 1 h 5 min', the reset time,
// burn per hour, source and confidence as a badge, age of the sample, and the count of
// leases on it. A sample older than `fresh_min` greys the bar. No account id, no email,
// no plan price."
//
// Section 7.7:
// "What the UI shows per pool: Used, committed, forecast time to the line with its
// confidence, reserve band, drain state, and the list of tasks that wait for this pool
// with their reason."
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

function formatDurationSecs(secs) {
  const s = Math.max(0, Math.round(Number(secs) || 0));
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  const remM = m % 60;
  if (h === 0) return `${m} min`;
  if (remM === 0) return `${h} h`;
  return `${h} h ${remM} min`;
}

function formatResetClock(resetsAtSec, nowSec) {
  const r = Number(resetsAtSec) || 0;
  if (!r) return "no reset";
  const d = new Date(r * 1000);
  const hh = String(d.getUTCHours()).padStart(2, "0");
  const mm = String(d.getUTCMinutes()).padStart(2, "0");
  if (nowSec && r > nowSec && r - nowSec > 24 * 3600) {
    const month = d.toLocaleString("en-US", { month: "short", timeZone: "UTC" });
    const day = d.getUTCDate();
    return `resets ${day} ${month} ${hh}:${mm}Z`;
  }
  return `resets ${hh}:${mm}Z`;
}

function stateBadgeClass(state) {
  switch (String(state || "unknown").toLowerCase()) {
    case "open":
      return "pool-st-open";
    case "throttled":
      return "pool-st-throttled";
    case "drain":
      return "pool-st-drain";
    case "closed":
    case "paused":
      return "pool-st-closed";
    default:
      return "pool-st-unknown";
  }
}

function renderQueuedTasks(waiting) {
  if (!Array.isArray(waiting) || waiting.length === 0) {
    return "";
  }
  const items = waiting
    .map((t) => {
      const id = esc(t.id || "?");
      const title = t.title ? ` ${esc(t.title)}` : "";
      const reason = esc(t.reason || "waiting");
      return `<li class="pool-wait-item"><code>${id}</code>${title} <span class="pool-wait-why">${reason}</span></li>`;
    })
    .join("");
  return `<ul class="pool-waiting" aria-label="Tasks waiting on this pool">${items}</ul>`;
}

export function renderPoolRow(p, nowSec, idx = 0) {
  if (!p || typeof p !== "object") return "";
  const state = String(p.state || "unknown").toLowerCase();
  const used = Math.max(0, Math.min(100, Number(p.used_pct) || 0));
  const committed = Math.max(0, Math.min(100 - used, Number(p.committed_pct) || 0));
  const line = Math.max(0, Math.min(100, Number(p.line_pct ?? p.line ?? 80)));
  const reserve = Math.max(0, Math.min(100, Number(p.reserve_pct ?? (100 - line))));
  const burnHr = Number(
    p.burn_pct_per_hour ?? ((Number(p.burn_pct_per_min) || 0) * 60)
  );
  const conf = Number(p.confidence) || 0;
  const band = esc(p.band || (conf >= 0.8 ? "high" : conf >= 0.5 ? "medium" : "low"));
  const source = esc(p.source || "inferred");
  const ageSecs = Number(p.age_secs ?? 0);
  const freshMin = Number(p.fresh_min ?? 30);
  const isStale = Boolean(p.stale) || ageSecs > freshMin * 60;
  const leases = Number(p.active_leases ?? 0);
  const forecastLabel = esc(
    p.forecast_label ||
      (p.resets_first
        ? `resets first, in ${formatDurationSecs(Math.max(0, (p.resets_at || 0) - (nowSec || 0)))}`
        : p.t_line
          ? `reaches the line in ${formatDurationSecs(Math.max(0, p.t_line - (nowSec || 0)))}`
          : "steady")
  );
  const resetText = esc(formatResetClock(p.resets_at, nowSec));
  const staleCls = isStale ? " pool-stale" : "";
  const drainCls = p.drain || state === "drain" ? " pool-drain" : "";
  const patId = `pool-hatch-${idx}`;
  const reserveX = Math.max(0, 100 - reserve);

  return `<article class="pool-row${staleCls}${drainCls}" data-pool="${esc(p.pool)}" data-owner="${esc(p.owner)}" data-state="${esc(state)}">
    <header class="pool-head">
      <span class="pool-name"><code>${esc(p.pool)}</code> <span class="pool-owner">${esc(p.owner)}</span></span>
      <span class="state ${stateBadgeClass(state)}">${esc(state)}</span>
      <span class="pool-badge" title="source ${source}, confidence ${conf.toFixed(2)} (${band})">${source} · ${band} (${Math.round(conf * 100)}%)</span>
      <span class="pool-leases">${leases} ${leases === 1 ? "lease" : "leases"}</span>
      <span class="pool-age${isStale ? " is-stale" : ""}">sample ${esc(formatDurationSecs(ageSecs))} old</span>
    </header>
    <div class="pool-bar${isStale ? " is-greyed" : ""}" role="meter" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${Math.round(used)}"
      data-used="${used.toFixed(1)}" data-committed="${committed.toFixed(1)}" data-line="${line.toFixed(0)}" data-reserve="${reserve.toFixed(0)}">
      <svg class="pool-meter-svg" viewBox="0 0 100 10" preserveAspectRatio="none" aria-hidden="true">
        <defs>
          <pattern id="${patId}" width="3" height="3" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width="1.5" height="3" class="pool-svg-hatch"/>
          </pattern>
        </defs>
        <rect x="0" y="0" width="100" height="10" class="pool-svg-track"/>
        ${reserve > 0 ? `<rect x="${reserveX.toFixed(2)}" y="0" width="${reserve.toFixed(2)}" height="10" class="pool-svg-reserve"/>` : ""}
        <rect x="0" y="0" width="${used.toFixed(2)}" height="10" class="pool-svg-used"/>
        ${committed > 0 ? `<rect x="${used.toFixed(2)}" y="0" width="${committed.toFixed(2)}" height="10" fill="url(#${patId})" class="pool-svg-committed"/>` : ""}
        <line x1="${line.toFixed(2)}" y1="0" x2="${line.toFixed(2)}" y2="10" class="pool-svg-line"/>
      </svg>
      <div class="pool-bar-legend">
        <span class="pool-bar-used" data-pct="${used.toFixed(1)}">${used.toFixed(1)}% used</span>
        ${committed > 0 ? `<span class="pool-bar-committed" data-pct="${committed.toFixed(1)}">+${committed.toFixed(1)}% committed</span>` : ""}
        <span class="pool-bar-line" data-line="${line.toFixed(0)}">line ${line.toFixed(0)}%</span>
        ${reserve > 0 ? `<span class="pool-bar-reserve" data-reserve="${reserve.toFixed(0)}">reserve ${reserve.toFixed(0)}%</span>` : ""}
      </div>
    </div>
    <footer class="pool-foot">
      <span class="pool-forecast">${forecastLabel}</span>
      <span class="pool-burn">${burnHr.toFixed(1)}%/h</span>
      <span class="pool-reset">${resetText}</span>
    </footer>
    ${renderQueuedTasks(p.waiting_tasks)}
  </article>`;
}

export function renderPools(state) {
  const pools = Array.isArray(state)
    ? state
    : (state && Array.isArray(state.pools) ? state.pools : []);
  const nowSec = state && typeof state.as_of === "number" ? state.as_of : Math.floor(Date.now() / 1000);
  if (!pools.length) {
    return `<section class="panel pools-panel" id="pools"><h2 class="panel-h">Quota pools</h2><p class="empty">No quota pools reported yet. Pools appear when members report meter or burn estimates.</p></section>`;
  }
  const rows = pools.map((p, i) => renderPoolRow(p, nowSec, i)).join("\n");
  return `<section class="panel pools-panel pools-view" id="pools" aria-label="Quota pools">
    <h2 class="panel-h">Quota pools <span class="count">${pools.length}</span></h2>
    <div class="pools-list">${rows}</div>
  </section>`;
}
