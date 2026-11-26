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
    return `<section class="panel tasks-panel"><h2 class="panel-h">Orders &amp; tasks</h2><p class="empty">No tasks in queue. Tasks enter here when work orders are queued.</p></section>`;
  }

  const activeLeaseByTask = new Map();
  for (const l of leases) {
    if (l && (l.state === "active" || l.state === "suspect")) {
      activeLeaseByTask.set(l.task_id, l);
    }
  }

  const rows = tasks.map((t) => {
    const lease = activeLeaseByTask.get(t.id);
    const holder = lease ? `${esc(lease.owner)}/${esc(lease.worker || "-")}${lease.state === "suspect" ? " (suspect)" : ""}` : "unleased";
    const score = t.score != null ? `score ${esc(t.score)}` : "";
    const feat = t.feature ? `<span class="chip chip-feat"><span>${esc(t.feature)}</span></span>` : "";
    const cp = t.latest_checkpoint && t.latest_checkpoint.note
      ? `<div class="task-cp">cp #${esc(t.latest_checkpoint.seq)}: ${esc(t.latest_checkpoint.note)}</div>`
      : "";
    return `<li class="task-row" data-task="${esc(t.id)}" data-state="${esc(t.state)}">
      <div class="cp-row-top">
        <span class="c-topic"><code>${esc(t.id)}</code> ${esc(t.title || "")}</span>
        ${stateBadge(t.state)}
      </div>
      <div class="cp-row-meta">
        ${feat}
        <span class="chip"><span>${esc(t.kind || "fix")} · ${esc(t.priority || "normal")}</span></span>
        <span class="t-meta">attempt ${esc(t.attempt || 1)}${score ? " · " + score : ""} · ${holder}</span>
      </div>
      ${cp}
    </li>`;
  }).join("");

  const openOffers = offers.filter((o) => o && o.state === "open");
  const offerRows = openOffers.map((o) => (
    `<li class="offer-row"><code>${esc(o.task_id)}</code> → <strong>${esc(o.to_owner)}</strong> <span class="chip"><span>${esc(o.class || "-")}</span></span></li>`
  )).join("");

  return `<section class="panel tasks-panel">
    <h2 class="panel-h">Orders &amp; tasks <span class="count">${tasks.length}</span></h2>
    <ul class="tasks-list">${rows}</ul>
    ${offerRows ? `<h3 class="panel-sub">Open offers <span class="count">${openOffers.length}</span></h3><ul class="offers-list">${offerRows}</ul>` : ""}
  </section>`;
}
