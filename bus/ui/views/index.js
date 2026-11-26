// Dashboard extension views registry (steps 8, 9, and 10).
"use strict";

import { renderTasks } from "./tasks.js";
import { renderPools } from "./pools.js";
import { renderLanding } from "./landing.js";
import { renderRuns } from "./runs.js";
import { renderBranches } from "./branches.js";
import { renderMetrics } from "./metrics.js";

export {
  renderTasks,
  renderPools,
  renderLanding,
  renderRuns,
  renderBranches,
  renderMetrics,
};

function isSlotEmpty(html) {
  return !html || html.includes('class="empty"');
}

export function renderViews(state, root) {
  const s = state || {};
  const parts = {
    tasks: renderTasks(s) || "",
    pools: renderPools(s) || "",
    landing: renderLanding(s) || "",
    runs: renderRuns(s) || "",
    branches: renderBranches(s) || "",
    metrics: renderMetrics(s) || "",
  };
  const doc = (root && root.ownerDocument) ? root.ownerDocument : (typeof document !== "undefined" ? document : null);
  if (doc && typeof doc.getElementById === "function") {
    const slots = [
      ["view-tasks", parts.tasks],
      ["view-pools", parts.pools],
      ["view-landing", parts.landing],
      ["view-runs", parts.runs],
      ["view-branches", parts.branches],
      ["view-metrics", parts.metrics],
    ];
    let updatedSlot = false;
    for (const [id, html] of slots) {
      const node = doc.getElementById(id);
      if (node) {
        node.innerHTML = html;
        node.classList.toggle("is-empty", isSlotEmpty(html));
        updatedSlot = true;
      }
    }
    const container = root || doc.getElementById("views");
    if (container && !updatedSlot) {
      container.innerHTML = Object.values(parts).filter(Boolean).join("\n");
    }
  }
  return parts;
}

if (typeof globalThis !== "undefined") {
  globalThis.sommelierRenderViews = renderViews;
}
