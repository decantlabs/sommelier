// Landing queue and batches view (step 10a).
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function landBadge(state) {
  const st = String(state || "queued").toLowerCase();
  const cls = st === "landed"
    ? "st-result"
    : st === "failed" || st === "conflict"
      ? "st-open"
      : st === "batched" || st === "held"
        ? "st-accepted"
        : "st-declined";
  return `<span class="state ${cls}">${esc(st)}</span>`;
}

export function renderLanding(state) {
  if (!state || typeof state !== "object") return "";
  const landing = Array.isArray(state.landing) ? state.landing : [];
  const batches = Array.isArray(state.batches) ? state.batches : [];

  if (!landing.length && !batches.length) {
    return `<div class="panel landing-panel"><h2 class="panel-h">Landing</h2><p class="empty">Landing queue is empty.</p></div>`;
  }

  const rows = landing.map((l) => {
    const shaShort = String(l.sha || "").slice(0, 10);
    const batch = l.batch_id ? `batch ${esc(l.batch_id)}` : "-";
    return `<li class="landing-row" data-ref="${esc(l.ref || "")}" data-state="${esc(l.state || "")}">
      <span class="c-topic"><code>${esc(l.ref || "")}</code> <code>${esc(shaShort)}</code></span>
      ${landBadge(l.state)}
      <span class="chip"><span>${esc(l.priority || "normal")} · tries ${esc(l.tries || 0)}</span></span>
      <span class="r-foot">${batch}</span>
    </li>`;
  }).join("");

  const batchRows = batches.map((b) => {
    const count = Array.isArray(b.members) ? b.members.length : 0;
    return `<li class="batch-row" data-batch="${esc(b.batch_id || "")}">
      <span class="c-topic"><code>${esc(b.batch_id || "")}</code> (${count} branches)</span>
      ${landBadge(b.result || "running")}
      <span class="r-foot">verifications ${esc(b.verifications || 0)}</span>
    </li>`;
  }).join("");

  return `<div class="panel landing-panel">
    <h2 class="panel-h">Landing <span class="count">${landing.length}</span></h2>
    ${rows ? `<ul class="landing-list">${rows}</ul>` : ""}
    ${batchRows ? `<h3 class="panel-sub">Batches (${batches.length})</h3><ul class="batches-list">${batchRows}</ul>` : ""}
  </div>`;
}
