// Metrics and regression view (step 10b).
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function fmtNum(v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return "-";
  return Number.isInteger(n) ? String(n) : n.toFixed(2);
}

function statusBadge(check) {
  const st = check && check.status ? check.status : "baseline_good";
  if (st === "regression_confirmed") {
    return `<span class="state st-open" title="Regression confirmed">REGRESSION (${fmtNum(check.worse_pct)}%)</span>`;
  }
  if (st === "confirm_runs_needed") {
    return `<span class="state st-accepted" title="Confirm runs scheduled">confirming (${fmtNum(check.worse_pct)}%)</span>`;
  }
  if (st === "need_more_runs") {
    return `<span class="state st-declined" title="Below min_runs">${esc(check.have_runs)}/${esc(check.min_runs)} runs</span>`;
  }
  if (check && check.ignored_as_noise) {
    return `<span class="state st-result" title="Within MAD noise guard">good (noise)</span>`;
  }
  return `<span class="state st-result">good</span>`;
}

export function renderMetrics(state) {
  if (!state || typeof state !== "object") return "";
  const baselines = Array.isArray(state.baselines) ? state.baselines : [];
  const metrics = Array.isArray(state.metrics) ? state.metrics : [];
  const checks = state.regress && Array.isArray(state.regress.checks) ? state.regress.checks : [];

  if (!baselines.length && !metrics.length && !checks.length) {
    return `<section class="panel metrics-panel"><h2 class="panel-h">Metrics &amp; baselines</h2><p class="empty">No metrics recorded yet.</p></section>`;
  }

  const baseRows = baselines.map((b) => {
    const range = b.from_sha && b.to_sha
      ? (b.from_sha === b.to_sha ? esc(b.to_sha.slice(0, 10)) : `${esc(b.from_sha.slice(0, 10))}..${esc(b.to_sha.slice(0, 10))}`)
      : "-";
    return `<li class="metric-base">
      <div class="cp-row-top">
        <span class="c-topic">${esc(b.name)}</span>
        <span class="m-val">median <strong>${fmtNum(b.median)}</strong></span>
      </div>
      <div class="cp-row-meta">
        <span class="chip"><span>${esc(b.machine_class)}/${esc(b.scenario)}</span></span>
        <span class="r-foot">${esc(b.n)} commits · <code>${range}</code></span>
      </div>
    </li>`;
  }).join("");

  const checkRows = checks.map((c) => {
    const shaShort = String(c.sha || "").slice(0, 10);
    const baseText = c.base != null ? `base ${fmtNum(c.base)} → ` : "";
    const newText = c.new != null ? `new ${fmtNum(c.new)}` : "";
    return `<li class="metric-check" data-status="${esc(c.status || "")}" data-sha="${esc(c.sha || "")}">
      <div class="cp-row-top">
        <span class="c-topic"><code>${esc(shaShort)}</code> ${esc(c.metric || "")}</span>
        ${statusBadge(c)}
      </div>
      <div class="cp-row-meta">
        <span class="chip"><span>${esc(c.machine_class || "default")}/${esc(c.scenario || "default")}</span></span>
        <span class="r-foot">${baseText}${newText}</span>
      </div>
    </li>`;
  }).join("");

  return `<section class="panel metrics-panel">
    <h2 class="panel-h">Metrics &amp; baselines <span class="count">${metrics.length}</span></h2>
    ${baseRows ? `<ul class="metrics-baselines">${baseRows}</ul>` : ""}
    ${checkRows ? `<h3 class="panel-sub">Regression checks <span class="count">${checks.length}</span></h3><ul class="metrics-checks">${checkRows}</ul>` : ""}
  </section>`;
}
