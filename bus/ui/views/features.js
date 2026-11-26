// Feature Map & Dependency Graph view.
"use strict";

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

const FSTATE = {
  mode: "graph",      // "graph" | "matrix"
  area: "",           // "" | area id
  status: "all",      // "all" | "works" | "fails" | "unknown" | "open"
  query: "",
  selectedId: "",
  lastState: null,
};

function titleCase(s) {
  return String(s || "").replace(/[_-]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}

export function normalizeFeatures(state) {
  if (!state || typeof state !== "object") return { areas: [], features: [], machines: [] };
  const raw = state.features;
  let list = [];
  let areaMeta = new Map();
  if (Array.isArray(raw)) {
    list = raw;
  } else if (raw && typeof raw === "object") {
    if (Array.isArray(raw.areas)) {
      for (const a of raw.areas) {
        if (a && a.id) areaMeta.set(a.id, a.title || titleCase(a.id));
      }
    }
    if (Array.isArray(raw.features)) list = raw.features;
  }
  const machineSet = new Set();
  const areaOrder = [];
  const areaCounts = new Map();
  const features = list.filter((f) => f && f.id).map((f) => {
    const areaId = f.area || String(f.id).split(".")[0] || "general";
    if (!areaMeta.has(areaId)) areaMeta.set(areaId, f.area_title || titleCase(areaId));
    if (!areaCounts.has(areaId)) {
      areaOrder.push(areaId);
      areaCounts.set(areaId, { works: 0, fails: 0, unknown: 0, total: 0 });
    }
    const stMap = (f.state && typeof f.state === "object") ? f.state : {};
    const vals = Object.entries(stMap);
    for (const [mk] of vals) machineSet.add(mk);
    let agg = "unknown";
    if (vals.some(([, v]) => v === "fails")) agg = "fails";
    else if (vals.length && vals.every(([, v]) => v === "works")) agg = "works";

    const counts = areaCounts.get(areaId);
    counts[agg]++;
    counts.total++;

    return {
      id: String(f.id),
      area: areaId,
      areaTitle: areaMeta.get(areaId),
      title: String(f.title || f.id),
      openOrders: Number(f.open_orders ?? ((Number(f.open_tasks) || 0) + (Number(f.open_runs) || 0))),
      openTasks: Number(f.open_tasks || 0),
      openRuns: Number(f.open_runs || 0),
      state: stMap,
      agg,
      depends: Array.isArray(f.depends) ? f.depends.map(String) : [],
      how: f.how ? String(f.how) : "",
      paths: Array.isArray(f.paths) ? f.paths.map(String) : [],
      proof: Array.isArray(f.proof) ? f.proof : [],
    };
  });

  const areas = areaOrder.map((id) => ({
    id,
    title: areaMeta.get(id) || titleCase(id),
    ...areaCounts.get(id),
  }));

  return { areas, features, machines: [...machineSet].sort() };
}

function statusPill(st, label) {
  const txt = label || st;
  return `<span class="f-badge f-st-${esc(st)}">${esc(txt)}</span>`;
}

function collectChain(features, startId) {
  const byId = new Map(features.map((f) => [f.id, f]));
  const rev = new Map();
  for (const f of features) {
    for (const d of f.depends) {
      if (!rev.has(d)) rev.set(d, []);
      rev.get(d).push(f.id);
    }
  }
  const upstream = new Set();
  const qUp = [...(byId.get(startId)?.depends || [])];
  while (qUp.length) {
    const id = qUp.shift();
    if (upstream.has(id) || !byId.has(id)) continue;
    upstream.add(id);
    for (const d of byId.get(id).depends) qUp.push(d);
  }
  const downstream = new Set();
  const qDown = [...(rev.get(startId) || [])];
  while (qDown.length) {
    const id = qDown.shift();
    if (downstream.has(id) || !byId.has(id)) continue;
    downstream.add(id);
    for (const d of rev.get(id) || []) qDown.push(d);
  }
  return { upstream, downstream, rev };
}

function computeGraphLayout(features, areas) {
  const byId = new Map(features.map((f) => [f.id, f]));
  const hasAnyEdges = features.some((f) => f.depends.some((d) => byId.has(d)));
  const NODE_W = 264;
  const NODE_H = 92;
  const COL_GAP = 76;
  const ROW_GAP = 16;
  const PAD_X = 24;
  const PAD_Y = 24;

  const pos = new Map();
  const headers = [];

  if (!hasAnyEdges) {
    // Clean area columns when the hub does not include dependency edges yet.
    let colIdx = 0;
    let maxRows = 0;
    const areaGroups = areas.map((a) => ({
      area: a,
      items: features.filter((f) => f.area === a.id),
    })).filter((g) => g.items.length);

    for (const g of areaGroups) {
      const x = PAD_X + colIdx * (NODE_W + COL_GAP * 0.55);
      headers.push({ x, y: PAD_Y, w: NODE_W, title: g.area.title, count: g.items.length });
      g.items.forEach((f, r) => {
        pos.set(f.id, { x, y: PAD_Y + 42 + r * (NODE_H + ROW_GAP), col: colIdx });
      });
      maxRows = Math.max(maxRows, g.items.length);
      colIdx++;
    }
    const width = Math.max(560, PAD_X * 2 + Math.max(1, colIdx) * (NODE_W + COL_GAP * 0.55) - COL_GAP * 0.55);
    const height = Math.max(240, PAD_Y * 2 + 42 + maxRows * (NODE_H + ROW_GAP));
    return { pos, headers, width, height, NODE_W, NODE_H, edges: [] };
  }

  // Topological depth assignment (handles any cycles gracefully via visited guard).
  const memo = new Map();
  const visiting = new Set();
  function depthOf(id) {
    if (memo.has(id)) return memo.get(id);
    if (visiting.has(id)) return 0;
    visiting.add(id);
    const f = byId.get(id);
    let d = 0;
    if (f) {
      for (const dep of f.depends) {
        if (byId.has(dep)) d = Math.max(d, depthOf(dep) + 1);
      }
    }
    visiting.delete(id);
    memo.set(id, d);
    return d;
  }

  const cols = new Map();
  let maxCol = 0;
  for (const f of features) {
    const c = depthOf(f.id);
    maxCol = Math.max(maxCol, c);
    if (!cols.has(c)) cols.set(c, []);
    cols.get(c).push(f);
  }

  const areaRank = new Map(areas.map((a, i) => [a.id, i]));
  let maxRows = 0;
  for (let c = 0; c <= maxCol; c++) {
    const list = (cols.get(c) || []).sort((a, b) =>
      (areaRank.get(a.area) ?? 99) - (areaRank.get(b.area) ?? 99) || a.id.localeCompare(b.id)
    );
    maxRows = Math.max(maxRows, list.length);
    const x = PAD_X + c * (NODE_W + COL_GAP);
    list.forEach((f, r) => {
      pos.set(f.id, { x, y: PAD_Y + r * (NODE_H + ROW_GAP), col: c });
    });
  }

  const edges = [];
  for (const f of features) {
    const to = pos.get(f.id);
    if (!to) continue;
    for (const dep of f.depends) {
      const from = pos.get(dep);
      if (!from) continue;
      edges.push({ from: dep, to: f.id, x1: from.x + NODE_W, y1: from.y + NODE_H / 2, x2: to.x, y2: to.y + NODE_H / 2, fails: byId.get(dep)?.agg === "fails" || f.agg === "fails" });
    }
  }

  const width = Math.max(640, PAD_X * 2 + (maxCol + 1) * NODE_W + maxCol * COL_GAP);
  const height = Math.max(280, PAD_Y * 2 + maxRows * (NODE_H + ROW_GAP));
  return { pos, headers, width, height, NODE_W, NODE_H, edges };
}

function trunc(s, n) {
  const str = String(s || "");
  return str.length > n ? str.slice(0, n - 1) + "…" : str;
}

function renderGraphHtml(features, areas, selectedId) {
  const layout = computeGraphLayout(features, areas);
  const chain = selectedId ? collectChain(features, selectedId) : { upstream: new Set(), downstream: new Set() };

  const headerSvg = layout.headers.map((h) => `
    <g class="fg-col-head" transform="translate(${h.x}, ${h.y})">
      <rect x="0" y="0" width="${h.w}" height="28" rx="6" class="fg-col-pill"/>
      <text x="12" y="18" class="fg-col-title">${esc(h.title)}</text>
      <text x="${h.w - 12}" y="18" text-anchor="end" class="fg-col-count">${h.count}</text>
    </g>
  `).join("");

  const edgesSvg = layout.edges.map((e) => {
    const dx = Math.max(32, (e.x2 - e.x1) * 0.5);
    const d = `M ${e.x1} ${e.y1} C ${e.x1 + dx} ${e.y1}, ${e.x2 - dx} ${e.y2}, ${e.x2} ${e.y2}`;
    let cls = "fg-edge";
    let marker = "url(#fg-arr)";
    if (selectedId) {
      if (e.to === selectedId || (chain.upstream.has(e.from) && chain.upstream.has(e.to))) {
        cls += " is-upstream";
        marker = "url(#fg-arr-up)";
      } else if (e.from === selectedId || (chain.downstream.has(e.from) && chain.downstream.has(e.to))) {
        cls += " is-downstream";
        marker = "url(#fg-arr-down)";
      } else {
        cls += " is-dimmed";
      }
    } else if (e.fails) {
      cls += " is-fail";
      marker = "url(#fg-arr-fail)";
    }
    return `<path class="${cls}" d="${d}" marker-end="${marker}" data-from="${esc(e.from)}" data-to="${esc(e.to)}"/>`;
  }).join("");

  const nodesSvg = features.map((f) => {
    const p = layout.pos.get(f.id);
    if (!p) return "";
    let cls = `fg-node fg-st-${f.agg}`;
    if (selectedId) {
      if (f.id === selectedId) cls += " is-selected";
      else if (chain.upstream.has(f.id)) cls += " is-upstream";
      else if (chain.downstream.has(f.id)) cls += " is-downstream";
      else cls += " is-dimmed";
    }
    const machines = Object.entries(f.state);
    const dots = machines.slice(0, 6).map(([mk, st], i) => {
      const cx = layout.NODE_W - 14 - (machines.slice(0, 6).length - 1 - i) * 11;
      return `<circle cx="${cx}" cy="68" r="3.5" class="fg-mdot fg-mdot-${esc(st)}"><title>${esc(mk)}: ${esc(st)}</title></circle>`;
    }).join("");

    return `<g class="${cls}" tabindex="0" role="button" aria-label="${esc(f.id)} ${esc(f.title)}, ${esc(f.agg)}" data-feature-id="${esc(f.id)}" transform="translate(${p.x}, ${p.y})">
      <rect x="0" y="0" width="${layout.NODE_W}" height="${layout.NODE_H}" rx="10" class="fg-box"/>
      <circle cx="16" cy="20" r="4.5" class="fg-dot"/>
      <text x="27" y="24" class="fg-id">${esc(trunc(f.id, 27))}</text>
      <text x="14" y="45" class="fg-title">${esc(trunc(f.title, 31))}</text>
      <text x="14" y="71" class="fg-meta">${esc(f.area)}${f.openOrders ? ` · ${f.openOrders} open` : ""}</text>
      ${dots}
    </g>`;
  }).join("");

  return `<div class="fg-canvas">
    <svg class="fg-svg" viewBox="0 0 ${layout.width} ${layout.height}" width="${layout.width}" height="${layout.height}" role="img" aria-label="Feature dependency graph">
      <defs>
        <marker id="fg-arr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 1 2 L 9 5 L 1 8 z" class="fg-arr-fill"/></marker>
        <marker id="fg-arr-up" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 1 2 L 9 5 L 1 8 z" class="fg-arr-up"/></marker>
        <marker id="fg-arr-down" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 1 2 L 9 5 L 1 8 z" class="fg-arr-down"/></marker>
        <marker id="fg-arr-fail" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 1 2 L 9 5 L 1 8 z" class="fg-arr-fail"/></marker>
      </defs>
      ${headerSvg}
      ${edgesSvg}
      ${nodesSvg}
    </svg>
  </div>`;
}

function renderMatrixHtml(features, areas, machines, selectedId) {
  const groups = areas.map((a) => ({
    area: a,
    items: features.filter((f) => f.area === a.id),
  })).filter((g) => g.items.length);

  return `<div class="f-areas">${groups.map((g) => {
    const total = Math.max(1, g.items.length);
    const wCount = g.items.filter((x) => x.agg === "works").length;
    const fCount = g.items.filter((x) => x.agg === "fails").length;
    const wPct = (wCount / total) * 100;
    const fPct = (fCount / total) * 100;
    const rows = g.items.map((f) => {
      const mCells = machines.map((mk) => {
        const st = f.state[mk];
        return `<td>${st ? statusPill(st) : '<span class="f-none">—</span>'}</td>`;
      }).join("");
      const selCls = f.id === selectedId ? " is-selected" : "";
      return `<tr class="f-row${selCls}" data-feature-id="${esc(f.id)}" tabindex="0">
        <td><code class="f-id-cell">${esc(f.id)}</code><div class="f-title-cell">${esc(f.title)}</div></td>
        <td>${statusPill(f.agg)}</td>
        <td class="f-deps-cell">${f.depends.length ? f.depends.map((d) => `<code>${esc(d)}</code>`).join(" ") : '<span class="f-none">—</span>'}</td>
        <td class="f-num-cell">${f.openOrders || "0"}</td>
        ${mCells}
      </tr>`;
    }).join("");

    return `<section class="panel f-area-card">
      <header class="f-area-head">
        <div class="f-area-title-wrap">
          <h3>${esc(g.area.title)}</h3>
          <span class="count">${g.items.length} features</span>
        </div>
        <div class="f-area-bar-wrap">
          <svg class="f-area-bar" viewBox="0 0 100 6" preserveAspectRatio="none" aria-hidden="true">
            <rect x="0" y="0" width="100" height="6" class="f-bar-track"/>
            <rect x="0" y="0" width="${wPct.toFixed(1)}" height="6" class="f-bar-works"/>
            <rect x="${wPct.toFixed(1)}" y="0" width="${fPct.toFixed(1)}" height="6" class="f-bar-fails"/>
          </svg>
          <span class="f-area-stats">${wCount} works · ${fCount} fails</span>
        </div>
      </header>
      <div class="f-table-wrap">
        <table class="f-table">
          <thead><tr><th>Feature</th><th>Status</th><th>Depends</th><th>Orders</th>${machines.map((mk) => `<th><code>${esc(mk)}</code></th>`).join("")}</tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>
    </section>`;
  }).join("")}</div>`;
}

function renderInspectorHtml(feature, allFeatures) {
  if (!feature) {
    return `<aside class="panel f-inspector is-empty-insp" id="feature-inspector">
      <p class="empty">Select any feature node or matrix row to inspect dependencies, machine proof status, code paths, and test proofs.</p>
    </aside>`;
  }
  const { rev } = collectChain(allFeatures, feature.id);
  const downstream = rev.get(feature.id) || [];
  const machines = Object.entries(feature.state);

  return `<aside class="panel f-inspector" id="feature-inspector">
    <header class="f-insp-head">
      <div>
        <div class="f-insp-meta">
          <span class="chip"><span>${esc(feature.areaTitle)}</span></span>
          ${statusPill(feature.agg)}
        </div>
        <h3><code>${esc(feature.id)}</code></h3>
        <p class="f-insp-title">${esc(feature.title)}</p>
      </div>
      <div class="f-insp-actions">
        <button type="button" class="btn btn-quiet" data-topic="${esc(feature.id)}">Filter feed</button>
        <button type="button" class="btn btn-quiet" data-fclear-sel="1" aria-label="Close inspector"><svg class="ic" aria-hidden="true"><use href="#i-x"/></svg></button>
      </div>
    </header>
    ${feature.how ? `<p class="f-insp-how"><strong>How to verify:</strong> ${esc(feature.how)}</p>` : ""}
    <div class="f-insp-grid">
      <div class="f-insp-block">
        <h4>Machine proof matrix</h4>
        ${machines.length
          ? `<div class="f-insp-chips">${machines.map(([mk, st]) => `<span class="f-mach-pill"><code>${esc(mk)}</code>${statusPill(st)}</span>`).join("")}</div>`
          : `<p class="empty">No proof runs recorded on any machine yet.</p>`}
      </div>
      <div class="f-insp-block">
        <h4>Open orders</h4>
        <p class="f-insp-orders">${feature.openOrders} open (${feature.openTasks} tasks · ${feature.openRuns} runs)</p>
      </div>
      ${feature.depends.length ? `<div class="f-insp-block">
        <h4>Depends on (${feature.depends.length})</h4>
        <div class="f-insp-chips">${feature.depends.map((d) => `<button type="button" class="chip" data-feature-id="${esc(d)}"><span>${esc(d)}</span></button>`).join("")}</div>
      </div>` : ""}
      ${downstream.length ? `<div class="f-insp-block">
        <h4>Required by (${downstream.length})</h4>
        <div class="f-insp-chips">${downstream.map((d) => `<button type="button" class="chip" data-feature-id="${esc(d)}"><span>${esc(d)}</span></button>`).join("")}</div>
      </div>` : ""}
      ${feature.paths.length ? `<div class="f-insp-block">
        <h4>Code paths</h4>
        <div class="f-insp-chips">${feature.paths.map((p) => `<span class="chip"><span>${esc(p)}</span></span>`).join("")}</div>
      </div>` : ""}
      ${feature.proof.length ? `<div class="f-insp-block">
        <h4>Proof milestones</h4>
        <div class="f-insp-chips">${feature.proof.map((pr) => `<span class="chip"><span>${esc(pr.profile)} · ${esc(pr.scenario)} · ${esc(pr.milestone)}</span></span>`).join("")}</div>
      </div>` : ""}
    </div>
  </aside>`;
}

export function renderFeatures(state) {
  FSTATE.lastState = state;
  const { areas, features, machines } = normalizeFeatures(state);
  if (!features.length) {
    return `<section class="panel f-empty-card">
      <h2 class="panel-h">Feature map</h2>
      <p class="empty">No feature map is configured on this hub. Add <code>[features] file = "features.toml"</code> to your <code>--factory</code> config and define <code>[[area]]</code> and <code>[[feature]]</code> entries in the project repository to see capability coverage and dependencies here.</p>
    </section>`;
  }

  const worksN = features.filter((f) => f.agg === "works").length;
  const failsN = features.filter((f) => f.agg === "fails").length;
  const unkN = features.filter((f) => f.agg === "unknown").length;
  const ordersN = features.reduce((acc, f) => acc + f.openOrders, 0);

  const q = FSTATE.query.trim().toLowerCase();
  const filtered = features.filter((f) => {
    if (FSTATE.area && f.area !== FSTATE.area) return false;
    if (FSTATE.status === "open" && f.openOrders <= 0) return false;
    if (FSTATE.status !== "all" && FSTATE.status !== "open" && f.agg !== FSTATE.status) return false;
    if (q && !f.id.toLowerCase().includes(q) && !f.title.toLowerCase().includes(q) && !f.area.toLowerCase().includes(q)) return false;
    return true;
  });

  if (FSTATE.selectedId && !features.some((f) => f.id === FSTATE.selectedId)) {
    FSTATE.selectedId = "";
  }
  const selected = features.find((f) => f.id === FSTATE.selectedId) || null;

  const areaPills = `<button type="button" class="f-pill${FSTATE.area === "" ? " is-on" : ""}" data-farea="">All areas <span class="count">${features.length}</span></button>` +
    areas.map((a) => `<button type="button" class="f-pill${FSTATE.area === a.id ? " is-on" : ""}" data-farea="${esc(a.id)}">${esc(a.title)} <span class="count">${a.total}</span></button>`).join("");

  const statusPills = [
    ["all", `All (${features.length})`],
    ["fails", `Fails (${failsN})`],
    ["works", `Works (${worksN})`],
    ["unknown", `Unknown (${unkN})`],
    ["open", `Open orders (${ordersN})`],
  ].map(([k, lbl]) => `<button type="button" class="f-pill f-pill-${k}${FSTATE.status === k ? " is-on" : ""}" data-fstatus="${k}">${lbl}</button>`).join("");

  return `<div class="features-shell">
    <section class="panel f-toolbar">
      <div class="f-tool-top">
        <div class="f-summary">
          <span class="f-sum-item is-works"><b>${worksN}</b> works</span>
          <span class="f-sum-item is-fails"><b>${failsN}</b> fails</span>
          <span class="f-sum-item is-unknown"><b>${unkN}</b> unknown</span>
          <span class="f-sum-item"><b>${ordersN}</b> open orders</span>
        </div>
        <div class="f-tool-right">
          <input type="search" class="f-search" data-fsearch="1" value="${esc(FSTATE.query)}" placeholder="Filter features..." aria-label="Filter features">
          <div class="seg-ctrl" role="group" aria-label="Feature view mode">
            <button type="button" class="seg-btn${FSTATE.mode === "graph" ? " is-on" : ""}" data-fmode="graph">Graph</button>
            <button type="button" class="seg-btn${FSTATE.mode === "matrix" ? " is-on" : ""}" data-fmode="matrix">Matrix</button>
          </div>
        </div>
      </div>
      <div class="f-filter-bars">
        <div class="f-pills" role="group" aria-label="Filter by area">${areaPills}</div>
        <div class="f-pills" role="group" aria-label="Filter by status">${statusPills}</div>
      </div>
    </section>
    ${filtered.length
      ? (FSTATE.mode === "matrix" ? renderMatrixHtml(filtered, areas, machines, FSTATE.selectedId) : renderGraphHtml(filtered, areas, FSTATE.selectedId))
      : `<section class="panel"><p class="empty">No features match the active filter.</p></section>`}
    ${renderInspectorHtml(selected, features)}
  </div>`;
}

export function renderFeaturesSummary(state) {
  const { areas, features } = normalizeFeatures(state);
  if (!features.length) {
    return `<section class="panel snap-card">
      <div class="snap-head"><h2 class="panel-h">Feature map</h2><button type="button" class="btn btn-ghost" data-jump-tab="features">Open</button></div>
      <p class="empty">No <code>features.toml</code> configured yet.</p>
    </section>`;
  }
  const worksN = features.filter((f) => f.agg === "works").length;
  const failsN = features.filter((f) => f.agg === "fails").length;
  const rows = areas.slice(0, 5).map((a) => {
    const total = Math.max(1, a.total);
    const wPct = (a.works / total) * 100;
    const fPct = (a.fails / total) * 100;
    return `<li class="snap-area-row">
      <div class="snap-area-top"><span>${esc(a.title)}</span><span class="count">${a.works}/${a.total} works${a.fails ? ` · ${a.fails} fails` : ""}</span></div>
      <svg class="f-area-bar" viewBox="0 0 100 6" preserveAspectRatio="none" aria-hidden="true">
        <rect x="0" y="0" width="100" height="6" class="f-bar-track"/>
        <rect x="0" y="0" width="${wPct.toFixed(1)}" height="6" class="f-bar-works"/>
        <rect x="${wPct.toFixed(1)}" y="0" width="${fPct.toFixed(1)}" height="6" class="f-bar-fails"/>
      </svg>
    </li>`;
  }).join("");

  return `<section class="panel snap-card">
    <div class="snap-head">
      <h2 class="panel-h">Feature map <span class="count">${worksN}/${features.length} works${failsN ? ` · ${failsN} fails` : ""}</span></h2>
      <button type="button" class="btn btn-ghost" data-jump-tab="features">Graph →</button>
    </div>
    <ul class="snap-area-list">${rows}</ul>
  </section>`;
}

export function wireFeaturesEvents(container) {
  if (!container || container.dataset.fWired) return;
  container.dataset.fWired = "1";
  const rerender = () => {
    if (FSTATE.lastState) container.innerHTML = renderFeatures(FSTATE.lastState);
  };
  container.addEventListener("click", (e) => {
    const modeBtn = e.target.closest("[data-fmode]");
    if (modeBtn) { FSTATE.mode = modeBtn.dataset.fmode; rerender(); return; }
    const areaBtn = e.target.closest("[data-farea]");
    if (areaBtn) { FSTATE.area = areaBtn.dataset.farea; rerender(); return; }
    const stBtn = e.target.closest("[data-fstatus]");
    if (stBtn) { FSTATE.status = stBtn.dataset.fstatus; rerender(); return; }
    const clearSel = e.target.closest("[data-fclear-sel]");
    if (clearSel) { FSTATE.selectedId = ""; rerender(); return; }
    const node = e.target.closest("[data-feature-id]");
    if (node) {
      const id = node.dataset.featureId;
      FSTATE.selectedId = FSTATE.selectedId === id ? "" : id;
      rerender();
    }
  });
  container.addEventListener("keydown", (e) => {
    const node = e.target.closest("[data-feature-id]");
    if (node && (e.key === "Enter" || e.key === " ")) {
      e.preventDefault();
      const id = node.dataset.featureId;
      FSTATE.selectedId = FSTATE.selectedId === id ? "" : id;
      rerender();
    }
  });
  container.addEventListener("input", (e) => {
    if (e.target.matches("[data-fsearch]")) {
      FSTATE.query = e.target.value;
      rerender();
      const inp = container.querySelector("[data-fsearch]");
      if (inp) { inp.focus(); inp.setSelectionRange(inp.value.length, inp.value.length); }
    }
  });
}
