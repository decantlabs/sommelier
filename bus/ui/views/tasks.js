// Task queue view (step 8).
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function stateBadge(state) {
  const st = String(state || "ready").toLowerCase();
  const cls = st === "closed" || st === "delivered"
    ? "st-result"
    : st === "stuck"
      ? "st-open"
      : st === "leased" || st === "running"
        ? "st-accepted"
        : "st-declined";
  return `<span class="state ${cls}">${esc(st)}</span>`;
}

export function renderTasks(state) {
  if (!state || typeof state !== "object") return "";
  const tasks = Array.isArray(state.tasks) ? state.tasks : [];
  const leases = Array.isArray(state.leases) ? state.leases : [];
  const offers = Array.isArray(state.offers) ? state.offers : [];

  if (!tasks.length && !leases.length && !offers.length) {
    return `<div class="panel tasks-panel"><h2 class="panel-h">Tasks</h2><p class="empty">No tasks in queue.</p></div>`;
  }

  const activeLeaseByTask = new Map();
  for (const l of leases) {
    if (l && (l.state === "active" || l.state === "suspect")) {
      activeLeaseByTask.set(l.task_id, l);
    }
  }

  const rows = tasks.map((t) => {
    const lease = activeLeaseByTask.get(t.id);
    const holder = lease ? `${esc(lease.owner)}/${esc(lease.worker || "-")}${lease.state === "suspect" ? " (suspect)" : ""}` : "-";
    const score = t.score != null ? `score ${esc(t.score)}` : "";
    const cp = t.latest_checkpoint && t.latest_checkpoint.note
      ? `<div class="task-cp">cp #${esc(t.latest_checkpoint.seq)}: ${esc(t.latest_checkpoint.note)}</div>`
      : "";
    return `<li class="task-row" data-task="${esc(t.id)}" data-state="${esc(t.state)}">
      <span class="c-topic"><code>${esc(t.id)}</code> ${esc(t.title || "")}</span>
      ${stateBadge(t.state)}
      <span class="chip"><span>${esc(t.kind || "fix")} · ${esc(t.priority || "normal")}</span></span>
      <span class="t-meta">attempt ${esc(t.attempt || 1)} · ${score} · holder ${holder}</span>
      ${cp}
    </li>`;
  }).join("");

  const openOffers = offers.filter((o) => o && o.state === "open");
  const offerRows = openOffers.map((o) => (
    `<li class="offer-row"><code>${esc(o.task_id)}</code> → ${esc(o.to_owner)} (${esc(o.class || "-")})</li>`
  )).join("");

  return `<div class="panel tasks-panel">
    <h2 class="panel-h">Tasks <span class="count">${tasks.length}</span></h2>
    <ul class="tasks-list">${rows}</ul>
    ${offerRows ? `<h3 class="panel-sub">Open offers (${openOffers.length})</h3><ul class="offers-list">${offerRows}</ul>` : ""}
  </div>`;
}
