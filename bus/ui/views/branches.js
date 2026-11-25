// Branches view (step 10a).
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function branchBadge(state) {
  const st = String(state || "active").toLowerCase();
  const cls = st === "landed" || st === "delivered"
    ? "st-result"
    : st === "orphan"
      ? "st-open"
      : st === "landing" || st === "held"
        ? "st-accepted"
        : "st-declined";
  return `<span class="state ${cls}">${esc(st)}</span>`;
}

export function renderBranches(state) {
  if (!state || typeof state !== "object") return "";
  const branches = Array.isArray(state.branches) ? state.branches : [];

  if (!branches.length) {
    return `<div class="panel branches-panel"><h2 class="panel-h">Branches</h2><p class="empty">No branches tracked yet.</p></div>`;
  }

  const rows = branches.map((b) => {
    const shaShort = String(b.head_sha || "").slice(0, 10);
    const full = b.remote ? `${b.remote}/${b.ref || ""}` : (b.ref || "");
    return `<li class="branch-row" data-branch="${esc(full)}" data-state="${esc(b.state || "")}">
      <span class="c-topic"><code>${esc(full)}</code> <code>${esc(shaShort)}</code></span>
      ${branchBadge(b.state)}
      <span class="chip"><span>ahead ${esc(b.ahead || 0)} · behind ${esc(b.behind || 0)}</span></span>
      <span class="r-foot">${b.task_id ? `task ${esc(b.task_id)}` : "-"}</span>
    </li>`;
  }).join("");

  return `<div class="panel branches-panel">
    <h2 class="panel-h">Branches <span class="count">${branches.length}</span></h2>
    <ul class="branches-list">${rows}</ul>
  </div>`;
}
