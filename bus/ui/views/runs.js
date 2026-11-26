// Game runs view (step 10a).
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function runBadge(run) {
  if (run && run.registered === false) {
    return `<span class="state st-open">unregistered</span>`;
  }
  const out = String((run && run.outcome) || "pending").toLowerCase();
  const cls = out === "pass"
    ? "st-result"
    : out === "fail" || out === "refuse" || out === "error"
      ? "st-open"
      : "st-accepted";
  return `<span class="state ${cls}">${esc(out)}</span>`;
}

export function renderRuns(state) {
  if (!state || typeof state !== "object") return "";
  const runs = Array.isArray(state.runs) ? state.runs : [];

  if (!runs.length) {
    return `<section class="panel runs-panel"><h2 class="panel-h">Runs</h2><p class="empty">No runs recorded yet. Title execution results appear here.</p></section>`;
  }

  const rows = runs.map((r) => {
    const shaShort = String(r.sha || "").slice(0, 10);
    return `<li class="run-row" data-run="${esc(r.run_id || "")}" data-registered="${r.registered !== false}">
      <div class="cp-row-top">
        <span class="c-topic"><code>${esc(r.run_id || "")}</code> ${esc(r.label || "")}</span>
        ${runBadge(r)}
      </div>
      <div class="cp-row-meta">
        <span class="chip"><span>${esc(r.kind || "check")} · ${esc(r.machine || "-")}</span></span>
        <span class="r-foot"><code>${esc(shaShort || "-")}</code></span>
      </div>
    </li>`;
  }).join("");

  return `<section class="panel runs-panel">
    <h2 class="panel-h">Runs <span class="count">${runs.length}</span></h2>
    <ul class="runs-list">${rows}</ul>
  </section>`;
}
