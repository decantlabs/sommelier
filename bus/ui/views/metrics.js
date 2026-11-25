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
    return `<div class="panel metrics-panel"><h2 class="panel-h">Metrics</h2><p class="empty">No metrics recorded yet.</p></div>`;
  }

  const baseRows = baselines.map((b) => {
    const range = b.from_sha && b.to_sha
      ? (b.from_sha === b.to_sha ? esc(b.to_sha.slice(0, 10)) : `${esc(b.from_sha.slice(0, 10))}..${esc(b.to_sha.slice(0, 10))}`)
      : "-";
    return `<li class="metric-base">
      <span class="c-topic">${esc(b.name)}</span>
      <span class="chip"><span>${esc(b.machine_class)}/${esc(b.scenario)}</span></span>
      <span class="m-val">median <strong>${fmtNum(b.median)}</strong> (${esc(b.n)} commits, <code>${range}</code>)</span>
    </li>`;
  }).join("");

  const checkRows = checks.map((c) => {
    const shaShort = String(c.sha || "").slice(0, 10);
    const baseText = c.base != null ? `base ${fmtNum(c.base)} → ` : "";
    const newText = c.new != null ? `new ${fmtNum(c.new)}` : "";
    return `<li class="metric-check" data-status="${esc(c.status || "")}" data-sha="${esc(c.sha || "")}">
      <span class="c-topic"><code>${esc(shaShort)}</code> ${esc(c.metric || "")}</span>
      <span class="chip"><span>${esc(c.machine_class || "default")}/${esc(c.scenario || "default")}</span></span>
      ${statusBadge(c)}
      <span class="r-foot">${baseText}${newText}</span>
    </li>`;
  }).join("");

  return `<div class="panel metrics-panel">
    <h2 class="panel-h">Metrics &amp; baselines <span class="count">${metrics.length}</span></h2>
    ${baseRows ? `<ul class="claims metrics-baselines">${baseRows}</ul>` : ""}
    ${checkRows ? `<ul class="runs metrics-checks">${checkRows}</ul>` : ""}
  </div>`;
}
