// sommelier dashboard. Read only. Plain JavaScript, no build step, no dependencies.
//
// Works from the WebSocket stream alone: members come from HELLO and from any
// message, agents from STATUS, claims from CLAIM and RELEASE, run requests from
// RUN_REQUEST, ACCEPT, DECLINE and RUN_RESULT. Presence and the authoritative
// claim list come from the `who` and `claims` requests, which observers may send.
// When the hub serves GET /api/state, that snapshot paints first.
"use strict";

(() => {
  const STALE_MIN = 20;          // as the hub's roster: no STATUS for this long looks quiet
  const SUSPECT_SEC = 120;       // heartbeat late for a supervised worker
  const STALE_FRAME_MS = 90000;  // stale banner after 90 s without a frame
  // A participant whose agent sends with one shot commands holds no connection between sends,
  // so presence is a connection now or a sign of life in the last PRESENT_MIN minutes.
  const PRESENT_MIN = 30;
  // Online is a connection now, or a real sign of life in the last PRESENT_MIN minutes: a message
  // of any kind but STATUS, or a status row whose state or text changed. A row refreshed with
  // the same state and text (a heartbeat) is no sign of life. m.lastMsg never counts STATUS.
  const lastChange = (m) => { let t = 0; for (const a of S.agents.values()) if (a.owner === m.name && a.changed > t) t = a.changed; return t; };
  const lastSign = (m) => Math.max(m.lastMsg || 0, lastChange(m));
  const present = (m) => m.online || (lastSign(m) > 0 && now() - lastSign(m) < PRESENT_MIN * 60000);
  // The last real sign of an offline card, "last message 7 h ago" or "last status change 6 h ago".
  function lastSignHtml(m) {
    const msg = m.lastMsg || 0, chg = lastChange(m), seen = m.seenOnline || 0;
    if (!msg && !chg && !seen) return "";
    const [word, t] = chg > msg && chg >= seen ? ["last status change", chg] : msg >= seen ? ["last message", msg] : ["last seen", seen];
    return `${word} <time data-ago="${t}">${agoText(t)}</time>`;
  }
  const POLL_SECS = 20;          // who and claims refresh
  const REPLAY = 200;            // messages replayed on the first connect
  const FEED_MAX = 600;          // DOM rows kept in the feed
  const TOKEN_KEY = "sommelier.token";
  const HUB_KEY = "sommelier.hub";
  const FILTER_KEY = "sommelier.filters";
  const TAB_KEY = "sommelier.tab";
  const THEME_KEY = "sommelier.theme";
  const VALID_TABS = new Set(["overview", "people", "tasks", "pools", "landing", "runs", "branches", "metrics", "ledger"]);

  const KINDS = ["READY", "LANDED", "FIX", "STUCK", "FINDING", "TASK", "ACCEPT", "DECLINE",
    "RUN_REQUEST", "RUN_RESULT", "CHAT", "HELLO", "CLAIM", "RELEASE", "STATUS", "POLICY", "ALERT", "FACT", "OFFER", "TASK_UPDATE"];
  // kind colour families, see .k-good and friends in style.css
  const KIND_FAMILY = {
    READY: "good", LANDED: "good", FIX: "bad", STUCK: "bad", FINDING: "info", TASK: "work",
    ACCEPT: "work", DECLINE: "work", RUN_REQUEST: "run", RUN_RESULT: "run", HELLO: "bus",
    CLAIM: "bus", RELEASE: "bus", STATUS: "bus", POLICY: "bus", ALERT: "bad", FACT: "info",
    OFFER: "work", TASK_UPDATE: "work", CHAT: "chat",
  };
  const kindCls = (k) => "k-" + (KIND_FAMILY[k] || "chat");
  const HIDDEN_BY_DEFAULT = new Set(["STATUS", "FACT"]);
  const REPORT_KINDS = new Set(["READY", "FINDING", "RUN_RESULT", "STUCK", "LANDED"]);
  // What answers an offer (TASK, RUN_REQUEST), the same rule as swarm/bus-ledger.py.
  const DELIVER_KINDS = new Set(["READY", "RUN_RESULT", "FIX", "STUCK", "RELEASE", "LANDED"]);
  const ANSWER_KINDS = new Set(["ACCEPT", "DECLINE", ...DELIVER_KINDS]);
  // A later message of the offer's own sender that names it by #id and says it takes the work
  // over or withdraws it closes the offer: nobody waits on the addressee any more.
  const WITHDRAW = /\b(take|taking|took|taken)\b[^.;]{0,24}?\bover\b|\bwithdraw(n|s|ing)?\b|\bcancel(s|led|ed|ling)?\b|\bno longer needed\b|\btake (it|this) back\b/i;
  const ROSTER_SECS = 60;        // the token list is read again this often
  const CREW_INLINE = 4;         // up to this many workers stay inside the participant's card
  const REPORT_FRESH_MIN = 60;  // a report this recent is the headline of an otherwise idle participant
  const PERSON_SLOTS = 8;      // participant colours, see .hue-0 to .hue-7 in style.css
  const STATES = ["blocked", "suspect", "running", "starved", "idle", "own", "stale", "offline", "done"];
  const GROUP_LABEL = {
    blocked: "Blocked", suspect: "Suspect", running: "Running", starved: "Starved",
    idle: "Idle", own: "Own", stale: "Quiet", offline: "Offline", done: "Done",
  };

  // ---------- state ----------

  const S = {
    msgs: new Map(),        // id -> message
    lastId: 0,
    members: new Map(),     // name -> member
    agents: new Map(),      // owner \u0000 agent -> row
    claims: new Map(),      // topic -> {topic, holder, since}
    asks: new Map(),        // id of RUN_REQUEST or TASK -> {msg, accepted:[], declined:[], result}
    claimNote: new Map(),   // topic -> body of the CLAIM that took it
    reports: new Map(),     // participant -> latest READY, FINDING, RUN_RESULT, STUCK or LANDED
    replies: [],            // answers to offers: {from, kind, topic, id, ts, ids, body}
    roster: null,           // Set of names that hold a token now, from /api/state; null while unknown
    hubId: "",
    asOf: 0,
    snapshot: null,         // full /api/state snapshot for extension views
    derived: null,          // snapshot.derived from /api/state
    derivedWorkers: new Map(), // owner \u0000 agent -> derived worker row
    lastFrameAt: 0,         // local Date.now() of the last frame or snapshot
    staleBanner: false,
    skew: 0,                // hub clock minus local clock, ms
    whoKnown: false,
    conn: "idle",
    opened: false,
    failures: 0,
    filters: loadFilters(),
  };

  const $ = (id) => document.getElementById(id);
  const el = {
    gate: $("gate"), gateForm: $("gate-form"), gateToken: $("gate-token"), gateHub: $("gate-hub"),
    gateErr: $("gate-err"), board: $("board"), people: $("people"), claims: $("claims"),
    claimsN: $("claims-n"), runs: $("runs"), runsN: $("runs-n"), msgs: $("msgs"), feedN: $("feed-n"),
    feedEmpty: $("feed-empty"), kinds: $("kinds"), fFrom: $("f-from"), fTopic: $("f-topic"),
    fClear: $("f-clear"), pulse: $("pulse"), conn: $("conn"), connText: $("conn-text"),
    forget: $("forget"), bar: $("bar"), staleBanner: $("stale-banner"), views: $("views"),
    triage: $("triage"), wsTabs: $("ws-tabs"), themeToggle: $("theme-toggle"),
    board: $("board"),
  };

  let viewsModule = null;
  function invokeRenderViews() {
    const viewState = S.snapshot || {
      as_of: S.asOf || Math.floor(now() / 1000),
      derived: S.derived,
      tasks: [],
      pools: [],
      landing: [],
      runs: [],
      branches: [],
      metrics: [],
      baselines: [],
    };
    const fn = (viewsModule && typeof viewsModule.renderViews === "function")
      ? viewsModule.renderViews
      : (typeof globalThis !== "undefined" && typeof globalThis.sommelierRenderViews === "function")
        ? globalThis.sommelierRenderViews
        : null;
    if (fn) {
      fn(viewState, el.views);
      return;
    }
    try {
      import("./views/index.js")
        .then((mod) => {
          viewsModule = mod;
          if (mod && typeof mod.renderViews === "function") {
            mod.renderViews(S.snapshot || viewState, el.views);
          }
        })
        .catch(() => {});
    } catch (_) { /* vm context without dynamic import */ }
  }

  // ---------- storage, never throws ----------

  function store(key, val) {
    try { val == null ? localStorage.removeItem(key) : localStorage.setItem(key, val); } catch (_) { /* private mode */ }
  }
  function load(key) {
    try { return localStorage.getItem(key); } catch (_) { return null; }
  }
  function loadFilters() {
    let f = null;
    try { f = JSON.parse(load(FILTER_KEY) || "null"); } catch (_) { f = null; }
    return {
      off: new Set(f && Array.isArray(f.off) ? f.off : [...HIDDEN_BY_DEFAULT]),
      from: (f && typeof f.from === "string") ? f.from : "",
      topic: (f && typeof f.topic === "string") ? f.topic : "",
    };
  }
  function saveFilters() {
    store(FILTER_KEY, JSON.stringify({ off: [...S.filters.off], from: S.filters.from, topic: S.filters.topic }));
  }

  // ---------- small helpers ----------

  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const now = () => Date.now() + S.skew;
  const tsMs = (ts) => { const t = Date.parse(ts); return Number.isFinite(t) ? t : 0; };

  function ago(ms) {
    const s = Math.max(0, Math.round((now() - ms) / 1000));
    if (s < 45) return "now";
    const m = Math.round(s / 60);
    if (m < 60) return m + " min";
    const h = Math.floor(m / 60);
    if (h < 24) return h + " h" + (m % 60 && h < 6 ? " " + (m % 60) + " min" : "");
    const d = Math.floor(h / 24);
    return d + (d === 1 ? " day" : " days");
  }
  function agoText(ms) { const a = ago(ms); return a === "now" ? "just now" : a + " ago"; }
  function clock(ms) {
    const d = new Date(ms);
    const sameDay = new Date(now()).toDateString() === d.toDateString();
    const hm = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    return sameDay ? hm : d.toLocaleDateString([], { month: "short", day: "numeric" }) + " " + hm;
  }
  function fullTime(ms) { return new Date(ms).toLocaleString(); }

  // In order of first appearance on the bus, so every viewer sees the same colours.
  const hueSlot = new Map();
  function hueOf(name) {
    if (!hueSlot.has(name)) hueSlot.set(name, hueSlot.size);
    return "hue-" + (hueSlot.get(name) % PERSON_SLOTS);
  }

  const isCommit = (r) => /^[0-9a-f]{7,40}$/i.test(r) && /\d/.test(r);
  function parseRefs(refs, to) {
    const out = { ids: [], to: [...(to || [])], commits: [], branches: [], people: [], other: [] };
    for (const r of refs || []) {
      if (/^#\d+$/.test(r)) out.ids.push(+r.slice(1));
      else if (/^to:/i.test(r)) out.to.push(r.slice(3).toLowerCase());
      else if (isCommit(r)) out.commits.push(r);
      else if (S.members.has(r)) out.people.push(r);
      else if (/^[a-z]+:/i.test(r)) out.other.push(r);
      else out.branches.push(r);
    }
    out.to = [...new Set(out.to)];
    return out;
  }

  // ---------- HELLO: the machine line ----------

  const CORE = ["os", "gpu", "cpu", "ram", "agent", "capacity"];
  function splitTop(s) {
    const parts = []; let depth = 0, cur = "";
    for (const c of s) {
      if (c === "(" || c === "[") depth++;
      if (c === ")" || c === "]") depth = Math.max(0, depth - 1);
      if ((c === "," || c === "|") && depth === 0) { parts.push(cur); cur = ""; continue; }
      cur += c;
    }
    parts.push(cur);
    return parts.flatMap((p) => p.split(/\s+(?=[a-z_]+=)/i)).map((p) => p.trim()).filter(Boolean);
  }
  function guessKey(p) {
    if (/\b(windows|linux|macos|mac os|ubuntu|arch|fedora|debian|darwin)\b/i.test(p)) return "os";
    if (/\b(rtx|gtx|rx\s?\d|radeon|arc\s|geforce|quadro|gpu|apple m\d)/i.test(p)) return "gpu";
    if (/\b(cpu|ryzen|intel|core i\d|threadripper|xeon|epyc)\b/i.test(p)) return "cpu";
    if (/\b(claude|codex|hermes|cursor|gemini|aider|copilot|agent)\b/i.test(p)) return "agent";
    if (/\b\d+\s?(gb|gib)\b/i.test(p)) return "ram";
    return null;
  }
  function parseHello(body) {
    const rig = {}; const extra = []; let prev = null;
    for (const p of splitTop(String(body || ""))) {
      const m = p.match(/^([a-z][a-z _]{0,15}?)\s*[=:]\s*(.+)$/i);
      if (m) {
        const k = m[1].trim().toLowerCase();
        rig[k] = rig[k] ? rig[k] + ", " + m[2] : m[2];
        prev = k; continue;
      }
      const g = guessKey(p);
      if (g && !rig[g]) { rig[g] = p; prev = g; continue; }
      if (prev && !CORE.includes(prev)) { rig[prev] += ", " + p; continue; }
      extra.push(p);
    }
    return { rig, extra, raw: String(body || "") };
  }

  // ---------- model updates ----------

  function member(name, role) {
    let m = S.members.get(name);
    // A name without a token (revoked) is no participant: hand back a throwaway object.
    if (!m && S.roster && !S.roster.has(name)) return { name, role: role || "member", hello: null, lastMsg: 0, online: false, since: 0, seenOnline: 0, joined: 0 };
    if (!m) {
      m = { name, role: role || "member", hello: null, lastMsg: 0, online: false, since: 0, seenOnline: 0, joined: 0 };
      S.members.set(name, m);
    }
    if (role && role !== "hub" && role !== "observer") m.role = role;
    return m;
  }

  // The replies that answer an offer: an answer kind that refers to it by #id (from anyone but
  // the asker), or a later answer kind of an addressee on the same topic. An ACCEPT or DECLINE
  // that names other offers by #id belongs to those only.
  function buildRepliesByOffer() {
    const byOffer = new Map();
    const byTopicAndTo = new Map();
    for (const a of S.asks.values()) {
      if (!a.msg.topic) continue;
      for (const to of parseRefs(a.msg.refs, a.msg.to).to) {
        const key = a.msg.topic + "\u0000" + to;
        if (!byTopicAndTo.has(key)) byTopicAndTo.set(key, []);
        byTopicAndTo.get(key).push(a);
      }
    }
    for (const r of S.replies) {
      const matched = new Set();
      for (const id of r.ids) {
        const a = S.asks.get(id);
        if (a && a.msg.id < r.id && a.msg.from !== r.from) matched.add(id);
      }
      const blocksTopicMatch = (r.kind === "ACCEPT" || r.kind === "DECLINE")
        && r.ids.some((id) => S.asks.has(id));
      if (!blocksTopicMatch) {
        for (const a of byTopicAndTo.get(r.topic + "\u0000" + r.from) || []) {
          if (a.msg.id < r.id && a.msg.from !== r.from) matched.add(a.msg.id);
        }
      }
      for (const id of matched) {
        if (!byOffer.has(id)) byOffer.set(id, []);
        byOffer.get(id).push(r);
      }
    }
    return byOffer;
  }

  function recompute(a, repliesByOffer) {
    const m = a.msg;
    const rs = repliesByOffer.get(m.id) || [];
    a.replies = rs;
    a.accepted = rs.filter((r) => r.kind === "ACCEPT").map((r) => ({ who: r.from, id: r.id, ts: r.ts, body: r.body }));
    a.declined = rs.filter((r) => r.kind === "DECLINE").map((r) => ({ who: r.from, id: r.id, ts: r.ts, body: r.body }));
    const d = rs.filter((r) => DELIVER_KINDS.has(r.kind));
    const first = d.find((r) => r.kind === "RUN_RESULT") || d[0] || null;
    a.result = first ? (S.msgs.get(first.id) || null) : null;
    if (a.result) a.result._for = m.id;
    dirtyAsk.add(m.id);
  }

  // When the row last changed. The hub sends `changed`. Without it the dashboard compares with
  // the row it holds: a row never seen before has no change until it differs from the next one.
  function changedOf(prev, row, hubChanged) {
    if (hubChanged) return tsMs(hubChanged);
    if (!prev) return 0;
    return prev.state === row.state && prev.text === row.text ? prev.changed : row.ts;
  }

  function applyStatus(m) {
    const name = m.topic.slice("agent/".length) || "agent";
    const row = { owner: m.from, agent: name, state: "running", model: "", branch: "", text: m.body, ts: tsMs(m.ts), id: m.id, changed: 0 };
    for (const r of m.refs || []) {
      const i = r.indexOf(":");
      if (i < 0) continue;
      const k = r.slice(0, i).toLowerCase(), v = r.slice(i + 1);
      if (k === "state" && v) row.state = v.toLowerCase();
      else if (k === "model") row.model = v;
      else if (k === "branch") row.branch = v;
    }
    row.changed = changedOf(S.agents.get(m.from + "\u0000" + name), row, null);
    S.agents.set(m.from + "\u0000" + name, row);
    pendingBump.add(m.from + "\u0000" + name);
  }

  function ingest(m, live) {
    if (!m || !Number.isSafeInteger(m.id) || S.msgs.has(m.id)) return;
    S.msgs.set(m.id, m);
    if (m.id > S.lastId) S.lastId = m.id;
    const t = tsMs(m.ts);
    if (m.role !== "hub" && m.role !== "observer" && m.from) {
      hueOf(m.from);
      const mem = member(m.from, m.role);
      if (m.kind !== "STATUS") mem.lastMsg = Math.max(mem.lastMsg, t);
      if (!mem.joined) mem.joined = t;
    }
    switch (m.kind) {
      case "HELLO": member(m.from, m.role).hello = { ...parseHello(m.body), ts: t }; break;
      case "STATUS": if ((m.topic || "").startsWith("agent/")) applyStatus(m); break;
      case "CLAIM":
        if (m.topic && !S.claims.has(m.topic)) S.claims.set(m.topic, { topic: m.topic, holder: m.from, since: t });
        if (m.topic && m.body) S.claimNote.set(m.topic, m.body);
        break;
      case "RELEASE": S.claims.delete(m.topic); break;
      case "RUN_REQUEST": case "TASK":
        S.asks.set(m.id, { msg: m, accepted: [], declined: [], result: null, replies: [] });
        // an older unanswered offer on this topic may now count as replaced
        if (m.topic) for (const a of S.asks.values()) if (a.msg.topic === m.topic && a.msg.id < m.id) dirtyAsk.add(a.msg.id);
        break;
    }
    if (ANSWER_KINDS.has(m.kind) && m.role !== "hub") {
      S.replies.push({ from: m.from, kind: m.kind, topic: m.topic || "", id: m.id, ts: t, ids: parseRefs(m.refs).ids, body: m.body });
      const repliesByOffer = buildRepliesByOffer();
      for (const a of S.asks.values()) if (a.msg.id < m.id) recompute(a, repliesByOffer);
    }
    if (m.body && m.role !== "hub" && WITHDRAW.test(m.body)) {
      for (const id of parseRefs(m.refs).ids) {
        const a = S.asks.get(id);
        if (a && a.msg.from === m.from && !a.withdrawn) { a.withdrawn = { id: m.id, ts: t }; dirtyAsk.add(id); }
      }
    }
    if (REPORT_KINDS.has(m.kind) && m.role !== "hub") {
      const prev = S.reports.get(m.from);
      if (!prev || prev.id < m.id) S.reports.set(m.from, m);
    }
    queueFeed(m, live);
    schedule();
  }

  function applyWho(conns) {
    S.whoKnown = true;
    const seen = new Map();
    for (const c of conns || []) {
      if (c.role === "observer" || c.role === "hub") continue;
      const s = tsMs(c.since);
      const prev = seen.get(c.name);
      seen.set(c.name, prev ? Math.min(prev, s) : s);
      member(c.name, c.role);
    }
    for (const m of S.members.values()) {
      const on = seen.has(m.name);
      if (m.online && !on) m.seenOnline = now();
      m.online = on;
      if (on) m.since = seen.get(m.name);
    }
    schedule();
  }

  function applyClaims(list) {
    S.claims.clear();
    for (const c of list || []) {
      S.claims.set(c.topic, { topic: c.topic, holder: c.holder, since: tsMs(c.since_ts || c.since), idle: c.idle_secs });
    }
    schedule();
  }

  function applySnapshot(st) {
    if (!st || typeof st !== "object") return;
    S.snapshot = st;
    S.lastFrameAt = Date.now();
    if (typeof st.as_of === "number") S.asOf = st.as_of;
    if (st.derived && typeof st.derived === "object") {
      S.derived = st.derived;
      S.derivedWorkers.clear();
      for (const w of st.derived.workers || []) {
        if (w && w.owner && w.agent) {
          S.derivedWorkers.set(w.owner + "\u0000" + w.agent, w);
        }
      }
    }
    if (st.hub) {
      if (typeof st.hub.id === "string") S.hubId = st.hub.id;
      // the hub's id is its newest message id: the live stream resumes after it
      if (typeof st.hub.id === "number") S.lastId = Math.max(S.lastId, st.hub.id);
      const hn = tsMs(st.hub.now);
      if (hn) S.skew = hn - Date.now();
    }
    if (Array.isArray(st.members)) S.roster = new Set(st.members.filter((x) => x && x.name).map((x) => x.name));
    for (const x of st.members || []) {
      if (!x || !x.name || x.role === "observer" || x.role === "hub") continue;
      hueOf(x.name);
      const m = member(x.name, x.role);
      if (x.hello) {
        const body = typeof x.hello === "string" ? x.hello : (x.hello.body ?? "");
        const ts = typeof x.hello === "object" ? tsMs(x.hello.ts) : 0;
        if (typeof x.hello === "object" && !x.hello.body) {
          m.hello = { rig: { ...x.hello }, extra: [], raw: "", ts };
        } else {
          m.hello = { ...parseHello(body), ts };
        }
      }
      if (typeof x.online === "boolean") {
        S.whoKnown = true;
        m.online = x.online;
        if (x.online) m.since = tsMs(x.since); else m.seenOnline = tsMs(x.since);
      }
    }
    for (const a of st.agents || []) {
      if (S.roster && !S.roster.has(a.owner)) continue;
      const key = a.owner + "\u0000" + a.agent;
      const dw = S.derivedWorkers.get(key);
      const row = {
        owner: a.owner, agent: a.agent, state: (a.state || "running").toLowerCase(),
        derivedState: dw ? dw.state : null,
        ageSecs: dw && typeof dw.age_secs === "number" ? dw.age_secs : null,
        reason: dw ? dw.reason : null,
        model: a.model || "", branch: a.branch || "", text: a.text || "", ts: tsMs(a.ts), id: a.id || 0,
        stale: a.stale === true, changed: 0,
      };
      row.changed = changedOf(S.agents.get(key), row, a.changed);
      S.agents.set(key, row);
      member(a.owner);
    }
    if (Array.isArray(st.claims)) applyClaims(st.claims.map((c) => ({ ...c, since_ts: c.since_ts || c.since })));
    const recent = (st.recent || []).slice().sort((a, b) => a.id - b.id);
    for (const m of recent) {
      // The snapshot's agent rows are already the latest: do not let older STATUS lines win.
      if (m.kind === "STATUS") {
        const key = m.from + "\u0000" + (m.topic || "").slice(6);
        const cur = S.agents.get(key);
        if (cur && cur.ts >= tsMs(m.ts)) { S.msgs.set(m.id, m); S.lastId = Math.max(S.lastId, m.id); queueFeed(m, false); continue; }
      }
      ingest(m, false);
    }
    invokeRenderViews();
    schedule();
  }

  // ---------- derived ----------

  function agentState(a, owner) {
    const key = a.owner + "\u0000" + a.agent;
    const dw = S.derivedWorkers.get(key);
    const dState = a.derivedState || (dw && dw.state);
    if (owner && S.whoKnown && !present(owner)) {
      if (dState === "offline") return "offline";
      const base = STATES.includes(a.state) ? a.state : "running";
      if (base !== "done") return "stale";
    }
    if (dState && STATES.includes(dState)) {
      const ageMin = (now() - a.ts) / 60000;
      if (ageMin >= STALE_MIN && dState !== "done" && dState !== "offline") return "stale";
      if (dState === "running" && !isOwnRow(a) && (now() - a.ts) / 1000 > SUSPECT_SEC) return "suspect";
      return dState;
    }
    let s = STATES.includes(a.state) ? a.state : "running";
    const quiet = a.stale || (now() - a.ts) / 60000 >= STALE_MIN;
    if (s === "running" && quiet) s = "stale";
    else if (s === "running" && !isOwnRow(a) && (now() - a.ts) / 1000 > SUSPECT_SEC) s = "suspect";
    if (owner && S.whoKnown && !present(owner) && (s === "running" || s === "suspect" || s === "blocked" || s === "idle" || s === "starved")) s = "stale";
    return s;
  }

  function isFrameStale() {
    return S.lastFrameAt > 0 && (Date.now() - S.lastFrameAt) >= STALE_FRAME_MS;
  }

  function renderStaleBanner() {
    const stale = isFrameStale();
    S.staleBanner = stale;
    if (!el.staleBanner) return;
    if (!stale) {
      el.staleBanner.hidden = true;
      el.staleBanner.innerHTML = "";
      return;
    }
    const secs = Math.round((Date.now() - S.lastFrameAt) / 1000);
    el.staleBanner.hidden = false;
    el.staleBanner.innerHTML = `<strong>Stale view:</strong> no frame from the hub for ${secs} s.`;
  }

  // An offer nobody took (unanswered or declined) that its sender sent again later, with the same
  // kind, topic and addressee, is replaced by the newer one.
  function replacedBy(a) {
    if (a.result || a.accepted.length || !a.msg.topic) return 0;
    const to = parseRefs(a.msg.refs, a.msg.to).to;
    let by = 0;
    for (const b of S.asks.values()) {
      const n = b.msg;
      if (n.id > a.msg.id && n.kind === a.msg.kind && n.from === a.msg.from && n.topic === a.msg.topic
        && parseRefs(n.refs, n.to).to.some((x) => to.includes(x))) by = Math.max(by, n.id);
    }
    return by;
  }
  // Closed without an answer: taken back by its sender, or sent again.
  const isClosed = (a) => !a.result && (!!a.withdrawn || !!replacedBy(a));
  const isOpenRun = (a) => !a.result && !isClosed(a) && !(a.declined.length && !a.accepted.length);

  function askState(a) {
    if (a.result) return { cls: "st-result", text: "result in" };
    if (a.withdrawn) return { cls: "st-declined", text: "taken back" };
    const by = replacedBy(a);
    if (by) return { cls: "st-declined", text: "replaced by #" + by };
    if (a.accepted.length) return { cls: "st-accepted", text: "accepted by " + a.accepted.map((x) => x.who).join(", ") };
    if (a.declined.length) return { cls: "st-declined", text: "declined by " + a.declined.map((x) => x.who).join(", ") };
    return { cls: "st-open", text: "open" };
  }

  // ---------- rendering ----------

  let raf = 0;
  const pendingBump = new Set();
  const dirtyAsk = new Set();
  function schedule() { if (!raf) raf = requestAnimationFrame(render); }

  function render() {
    raf = 0;
    renderStaleBanner();
    renderTriage();
    renderPeople();
    invokeRenderViews();
    renderLedger();
    updateTabCounts();
    flushFeed();
    renderPulse();
  }

  function sortedMembers() {
    const rank = (m) => (m.role === "orchestrator" ? 0 : 1);
    return [...S.members.values()].sort((a, b) =>
      rank(a) - rank(b) || (b.online - a.online) || a.name.localeCompare(b.name));
  }

  // One line in the card: OS and GPU. The rest of the machine sits behind the toggle.
  function rigHtml(h) {
    if (!h) return `<p class="rig-none">No HELLO yet, so the machine is unknown.</p>`;
    const labels = { os: "OS", gpu: "GPU", cpu: "CPU", ram: "RAM", agent: "Agent", capacity: "Capacity" };
    const cell = (k) => `<div><dt>${labels[k]}</dt><dd>${esc(h.rig[k])}</dd></div>`;
    const top = ["os", "gpu"].filter((k) => h.rig[k]).map(cell);
    const more = CORE.filter((k) => h.rig[k] && k !== "os" && k !== "gpu").map(cell)
      .concat(Object.keys(h.rig).filter((k) => !CORE.includes(k)).map((k) => `<div><dt>${esc(k)}</dt><dd>${esc(h.rig[k])}</dd></div>`));
    if (!top.length && !more.length) return `<p class="rig-raw">${esc(h.raw)}</p>`;
    return `<dl class="rig">${top.join("")}</dl>` + (more.length || h.extra.length
      ? `<details class="rig-more"><summary>More from the HELLO</summary><dl class="rig">${more.join("")}</dl>${h.extra.length ? `<p>${esc(h.extra.join(", "))}</p>` : ""}</details>` : "");
  }
  function presenceHtml(m) {
    if (!S.whoKnown) {
      return m.lastMsg ? `<span class="presence">last message <time data-ago="${m.lastMsg}">${agoText(m.lastMsg)}</time></span>` : "";
    }
    if (m.online) return `<span class="presence on" title="${esc(fullTime(m.since))}">online since ${esc(clock(m.since))}</span>`;
    if (present(m)) return `<span class="presence on">active, ${lastSignHtml(m)}</span>`;
    const sign = lastSignHtml(m);
    return `<span class="presence">offline${sign ? ", " + sign : ""}</span>`;
  }

  function agentHtml(a, st) {
    const key = a.owner + "\u0000" + a.agent;
    const dw = S.derivedWorkers.get(key);
    const reason = a.reason || (dw && dw.reason) || "";
    const meta = [];
    if (a.branch) meta.push(`<span class="chip" title="branch ${esc(a.branch)}"><svg class="ic"><use href="#i-branch"/></svg><span>${esc(a.branch)}</span></span>`);
    if (a.model) meta.push(`<span class="a-model">${esc(a.model)}</span>`);
    const label = st === "stale" ? `quiet, ${a.state}` : (reason ? `${st} (${reason})` : st);
    const textPart = a.text ? esc(a.text) : `<span class="rig-none">no text</span>`;
    const reasonHtml = reason && (st === "starved" || st === "suspect") ? ` <span class="a-reason">(${esc(reason)})</span>` : "";
    const ageSecs = typeof a.ageSecs === "number" ? a.ageSecs : Math.max(0, Math.round((now() - a.ts) / 1000));
    return `<li class="agent s-${st}${openAgents.has(key) ? " open" : ""}" tabindex="0" data-key="${esc(key)}" data-age-secs="${ageSecs}"
      title="${esc(a.agent)}: ${esc(label)}, updated ${esc(fullTime(a.ts))}">
      <span class="node" aria-hidden="true"></span>
      <span class="a-name">${esc(a.agent)}</span>
      <span class="a-text">${textPart}${reasonHtml}</span>
      <span class="a-meta">${meta.join("")}</span>
      <time class="age" data-ago="${a.ts}" data-short="1">${ago(a.ts)}</time>
      <span class="sr">, ${esc(label)}</span>
    </li>`;
  }

  const openAgents = new Set();
  let doneOpen = new Set();

  // ---------- a participant is an agent too ----------

  // A STATUS named `main`, or named like its sender, is the participant's own line.
  const isOwnRow = (a) => a.agent === "main" || a.agent === a.owner;
  const firstPart = (s) => { const p = String(s || "").split(" | ")[0]; return p.length > 240 ? p.slice(0, 239) + "…" : p; };

  // What a participant is doing, from the stream alone.
  function activity(m) {
    const name = m.name;
    const holds = [...S.claims.values()].filter((c) => c.holder === name).sort((a, b) => b.since - a.since);
    const doing = [], waiting = [];
    for (const a of S.asks.values()) {
      if (isClosed(a)) continue;
      const acc = a.accepted.filter((x) => x.who === name).pop();
      if (acc) {
        const answered = S.replies.some((r) => r.from === name && DELIVER_KINDS.has(r.kind)
          && ((r.id > acc.id && r.topic === a.msg.topic) || r.ids.includes(acc.id) || r.ids.includes(a.msg.id)));
        // one ACCEPT that takes several offers at once is one piece of work: keep the newest offer
        const same = doing.findIndex((d) => d.acc.id === acc.id);
        if (!answered && same < 0) doing.push({ ask: a, acc });
        else if (!answered && doing[same].ask.msg.id < a.msg.id) doing[same] = { ask: a, acc };
        continue;
      }
      // answered: any ACCEPT, DECLINE or delivery of the addressee that refers to it or follows on its topic
      if (a.replies.some((r) => r.from === name)) continue;
      if (parseRefs(a.msg.refs, a.msg.to).to.includes(name)) waiting.push(a);
    }
    doing.sort((x, y) => y.acc.id - x.acc.id);
    waiting.sort((x, y) => y.msg.id - x.msg.id);
    const report = S.reports.get(name) || null;
    let own = null;
    const workers = [];
    for (const a of S.agents.values()) {
      if (a.owner !== name) continue;
      if (isOwnRow(a)) { if (!own || a.ts > own.ts) own = a; } else workers.push(a);
    }
    return { holds, doing, waiting, report, own, workers };
  }

  // One headline per participant: its own STATUS, else working, reported, idle or offline.
  function headline(m, act) {
    const off = S.whoKnown && !present(m);
    if (act.own) {
      const st = agentState(act.own, m);
      const word = {
        running: "Working", suspect: "Suspect", blocked: "Blocked", starved: "Starved",
        idle: "Idle", own: "Own mode", stale: "Quiet", offline: "Offline", done: "Done",
      }[st] || "Working";
      return { st, word, topic: act.own.branch, text: act.own.text, ts: act.own.ts, src: "own" };
    }
    if (off) {
      const last = Math.max(m.seenOnline || 0, lastSign(m));
      return { st: "offline", word: "Offline", text: "", ts: last, src: "off" };
    }
    if (act.doing.length) {
      const d = act.doing[0];
      return { st: "running", word: "Working on", topic: d.ask.msg.topic || "#" + d.ask.msg.id, text: firstPart(d.ask.msg.body), ts: d.acc.ts, src: "doing" };
    }
    if (act.holds.length) {
      const c = act.holds[0];
      return { st: "running", word: "Working on", topic: c.topic, text: S.claimNote.get(c.topic) || "holds the claim", ts: c.since, src: "hold" };
    }
    const r = act.report;
    if (r && (now() - tsMs(r.ts)) / 60000 < REPORT_FRESH_MIN) {
      return { st: "reported", word: "Reported", kind: r.kind, topic: r.topic, text: firstPart(r.body), ts: tsMs(r.ts), src: "report" };
    }
    return { st: "idle", word: "Idle", text: S.whoKnown ? "nothing open" : "", ts: m.since || m.lastMsg, src: "idle" };
  }

  function nowHtml(h) {
    const topic = h.topic ? `<span class="now-topic">${esc(h.topic)}</span>` : "";
    const kind = h.kind ? `<span class="m-kind ${kindCls(h.kind)}">${esc(h.kind)}</span>` : "";
    const age = h.ts ? `<time class="age" data-ago="${h.ts}" data-short="1" title="${esc(fullTime(h.ts))}">${ago(h.ts)}</time>` : "";
    return `<div class="now n-${h.st}">
      <span class="node" aria-hidden="true"></span>
      <div class="now-main">
        <p class="now-head"><span class="now-word">${esc(h.word)}</span>${kind}${topic}</p>
        ${h.text ? `<p class="now-text">${esc(h.text)}</p>` : ""}
      </div>
      ${age}
    </div>`;
  }

  function actRow(k, topic, textHtml, ts, extra) {
    return `<li class="act${extra || ""}"><span class="act-k">${k}</span><span class="act-topic">${esc(topic)}</span>
      <span class="act-text">${textHtml}</span><time class="age" data-ago="${ts}" data-short="1">${ago(ts)}</time></li>`;
  }

  function actsHtml(act, h) {
    const rows = [];
    act.doing.forEach((d, i) => {
      if (h.src === "doing" && i === 0) return;
      const m = d.ask.msg;
      rows.push({ ts: d.acc.ts, html: actRow("Running", m.topic || "#" + m.id, esc(firstPart(m.body)), d.acc.ts) });
    });
    act.holds.forEach((c, i) => {
      if (h.src === "hold" && i === 0) return;
      const idle = typeof c.idle === "number" && c.idle >= 300 ? `, quiet ${Math.round(c.idle / 60)} min` : "";
      rows.push({ ts: c.since, html: actRow("Holds", c.topic, esc((S.claimNote.get(c.topic) || "claimed") + idle), c.since) });
    });
    for (const a of act.waiting) {
      const m = a.msg, t = tsMs(m.ts);
      rows.push({ ts: t, html: actRow("Asked", m.topic || "#" + m.id,
        `<span class="m-kind ${kindCls(m.kind)}">${esc(m.kind)}</span> from ${esc(m.from)}, not answered yet: ${esc(firstPart(m.body))}`,
        t, " act-ask") });
    }
    const r = act.report;
    if (r && h.src !== "report") {
      rows.push({ ts: Infinity, html: actRow("Last report", r.topic || "",
        `<span class="m-kind ${kindCls(r.kind)}">${esc(r.kind)}</span> ${esc(firstPart(r.body))}`, tsMs(r.ts)) });
    }
    return rows.length ? `<ul class="acts">${rows.map((x) => x.html).join("")}</ul>` : "";
  }

  function workersHtml(m, list) {
    const groups = { blocked: [], suspect: [], running: [], starved: [], idle: [], own: [], stale: [], offline: [], done: [] };
    for (const a of list) {
      const st = agentState(a, m);
      (groups[st] || groups.running).push(a);
    }
    for (const g of Object.values(groups)) g.sort((x, y) => y.ts - x.ts || x.agent.localeCompare(y.agent));
    const tally = [
      groups.running.length ? `<span class="t-run"><b>${groups.running.length}</b>running</span>` : "",
      groups.suspect.length ? `<span class="t-block"><b>${groups.suspect.length}</b>suspect</span>` : "",
      groups.blocked.length ? `<span class="t-block"><b>${groups.blocked.length}</b>blocked</span>` : "",
      groups.starved.length ? `<span class="t-block"><b>${groups.starved.length}</b>starved</span>` : "",
      groups.idle.length ? `<span><b>${groups.idle.length}</b>idle</span>` : "",
      groups.own.length ? `<span><b>${groups.own.length}</b>own</span>` : "",
      groups.stale.length ? `<span><b>${groups.stale.length}</b>quiet</span>` : "",
      groups.done.length ? `<span><b>${groups.done.length}</b>done</span>` : "",
    ].join("");
    const body = STATES.filter((s) => groups[s].length).map((s) => {
      const rows = groups[s].map((a) => agentHtml(a, s)).join("");
      const hint = s === "stale" ? `<span class="hint">no STATUS for ${STALE_MIN} min${S.whoKnown && !present(m) ? " or owner offline" : ""}</span>` : "";
      const head = `<span>${GROUP_LABEL[s]}</span><span class="n">${groups[s].length}</span>${hint}`;
      if (s === "done") {
        return `<details class="grp grp-done done-fold" data-owner="${esc(m.name)}"${doneOpen.has(m.name) ? " open" : ""}>
          <summary class="grp-h">${head}<svg class="ic" aria-hidden="true"><use href="#i-chev"/></svg></summary>
          <ul class="agents">${rows}</ul></details>`;
      }
      return `<div class="grp grp-${s}"><h4 class="grp-h">${head}</h4><ul class="agents">${rows}</ul></div>`;
    }).join("");
    return `<section class="workers"><h4 class="workers-h"><span>Workers</span><span class="n">${list.length}</span><span class="tally">${tally}</span></h4>
      <div class="line">${body}</div></section>`;
  }

  function renderPeople() {
    const list = sortedMembers();
    if (!list.length) {
      el.people.innerHTML = S.opened
        ? `<div class="person person-empty"><svg viewBox="0 0 120 24" aria-hidden="true"><path d="M2 12h116"/><circle cx="14" cy="12" r="5"/><circle cx="46" cy="12" r="5"/><circle cx="78" cy="12" r="5"/><circle cx="106" cy="12" r="5"/></svg><h3>Nobody on the bus yet</h3><p>A participant appears here with its first message and its machine with a <strong>HELLO</strong>. What it works on comes from its claims, accepts and reports, and workers it runs show up with a <strong>STATUS</strong> on topic <code>agent/&lt;name&gt;</code>.</p></div>`
        : skeleton(2);
      return;
    }
    // keep open folds across renders
    doneOpen = new Set([...el.people.querySelectorAll("details.done-fold[open]")].map((d) => d.dataset.owner));
    const openRig = new Set([...el.people.querySelectorAll("details.rig-more[open]")].map((d) => d.closest(".person").dataset.name));

    const acts = new Map(list.map((m) => [m.name, activity(m)]));
    // Token holders who never showed up get one quiet line, not an empty card.
    const seen = (m) => m.online || m.hello || m.lastMsg || acts.get(m.name).own || acts.get(m.name).workers.length;
    const dormant = list.filter((m) => !seen(m));
    // Every participant first, side by side, so everyone's state is in view at once.
    // A long worker list gets its own full width section below the cards.
    const shown = list.filter(seen);
    const off = (m) => (S.whoKnown && !present(m) ? " is-off" : "");
    const cards = shown.map((m) => {
      const act = acts.get(m.name);
      const h = headline(m, act);
      const n = act.workers.length;
      const crewLink = n > CREW_INLINE
        ? `<a class="crew-link" href="#crew-${esc(m.name)}">${n} workers, ${act.workers.filter((w) => agentState(w, m) === "running").length} running<svg class="ic" aria-hidden="true"><use href="#i-arrow"/></svg></a>` : "";
      return `<article class="person${off(m)} ${hueOf(m.name)}" data-name="${esc(m.name)}">
        <header class="person-h">
          <div class="who"><h3>${esc(m.name)}</h3><span class="role">${esc(m.role)}</span>${presenceHtml(m)}</div>
          <div class="rig-slot">${rigHtml(m.hello)}</div>
        </header>
        ${nowHtml(h)}
        ${actsHtml(act, h) || '<div class="acts-none"></div>'}
        <div class="pfoot">${n && n <= CREW_INLINE ? workersHtml(m, act.workers) : ""}${crewLink}</div>
      </article>`;
    });
    const crews = shown.filter((m) => acts.get(m.name).workers.length > CREW_INLINE).map((m) =>
      `<article class="person crew${off(m)} ${hueOf(m.name)}" id="crew-${esc(m.name)}" data-name="${esc(m.name)}">
        <h3 class="crew-h">${esc(m.name)}<span>workers</span></h3>
        ${workersHtml(m, acts.get(m.name).workers)}
      </article>`);
    el.people.innerHTML = cards.join("") + crews.join("") + (dormant.length
      ? `<p class="dormant">Not on the bus yet: ${dormant.map((m) => `<span class="${hueOf(m.name)}"><span class="dot"></span>${esc(m.name)}</span>`).join("")}</p>` : "");

    for (const name of openRig) {
      const d = el.people.querySelector(`.person[data-name="${CSS.escape(name)}"] details.rig-more`);
      if (d) d.open = true;
    }
    for (const key of pendingBump) {
      const li = el.people.querySelector(`.agent[data-key="${CSS.escape(key)}"]`);
      if (li && S.opened) li.classList.add("bump");
    }
    pendingBump.clear();
  }

  function skeleton(n) {
    return Array.from({ length: n }, () => `<div class="person skel" aria-hidden="true">
      <i class="sk-title"></i><i class="sk-60"></i>
      <i class="sk-92 sk-gap"></i><i class="sk-85"></i><i class="sk-88"></i></div>`).join("");
  }

  function renderLedger() {
    const claims = [...S.claims.values()].filter((c) => !S.roster || S.roster.has(c.holder)).sort((a, b) => b.since - a.since);
    el.claimsN.textContent = claims.length || "";
    el.claims.innerHTML = claims.length ? claims.map((c) => `<li>
        <span class="c-topic" title="${esc(c.topic)}">${esc(c.topic)}</span>
        <span class="c-holder ${hueOf(c.holder)}"><span class="dot"></span>${esc(c.holder)}</span>
        <span class="c-since">${c.since ? `since ${esc(clock(c.since))}, <time data-ago="${c.since}">${agoText(c.since)}</time>` : ""}</span>
      </li>`).join("")
      : `<li class="empty">No topic is claimed. A <strong>CLAIM</strong> takes one, a <strong>RELEASE</strong> frees it.</li>`;

    const runs = [...S.asks.values()].filter((a) => a.msg.kind === "RUN_REQUEST")
      .sort((a, b) => (!!(a.result || isClosed(a)) - !!(b.result || isClosed(b))) || b.msg.id - a.msg.id).slice(0, 8);
    const open = runs.filter(isOpenRun).length;
    el.runsN.textContent = runs.length ? (open ? open + " open" : "") : "";
    el.runs.innerHTML = runs.length ? runs.map((a) => {
      const st = askState(a); const m = a.msg; const t = tsMs(m.ts);
      return `<li>
        <span class="r-top"><span class="c-topic">${esc(m.topic || "#" + m.id)}</span></span>
        <span class="state ${st.cls}">${esc(st.text)}</span>
        <span class="r-body">${esc(m.body)}</span>
        <span class="r-foot">asked by ${esc(m.from)}, <time data-ago="${t}">${agoText(t)}</time>${a.result ? `, result <a href="#m${a.result.id}" data-jump="${a.result.id}">#${a.result.id}</a>` : ""}, <a href="#m${m.id}" data-jump="${m.id}">#${m.id}</a></span>
      </li>`;
    }).join("")
      : `<li class="empty">No run requests yet. A <strong>RUN_REQUEST</strong> shows here until its <strong>RUN_RESULT</strong> is in.</li>`;
  }

  function renderPulse() {
    const mem = [...S.members.values()];
    const online = mem.filter(present).length;
    // every worker row, plus every participant whose own headline says working
    let running = 0, blocked = 0;
    for (const a of S.agents.values()) {
      if (!S.members.has(a.owner)) continue;
      const st = agentState(a, S.members.get(a.owner));
      if (st === "running") running++;
      if (st === "blocked") blocked++;
    }
    for (const m of mem) {
      const act = activity(m);
      if (!act.own && headline(m, act).st === "running") running++;
    }
    const openRuns = [...S.asks.values()].filter((a) => a.msg.kind === "RUN_REQUEST" && isOpenRun(a)).length;
    const hourAgo = now() - 3600e3;
    let hour = 0, oldest = Infinity;
    for (const m of S.msgs.values()) { const t = tsMs(m.ts); if (t >= hourAgo) hour++; oldest = Math.min(oldest, t); }
    const partial = S.msgs.size >= REPLAY && oldest >= hourAgo;
    const parts = [
      S.whoKnown ? `<span class="${online ? "go" : ""}"><b>${online}</b>of ${mem.length} active</span>` : `<span><b>${mem.length}</b>participants</span>`,
      `<span class="${running ? "go" : ""}"><b>${running}</b>agents running</span>`,
      blocked ? `<span class="hot"><b>${blocked}</b>blocked</span>` : "",
      `<span><b>${openRuns}</b>open ${openRuns === 1 ? "run" : "runs"}</span>`,
      `<span><b>${hour}${partial ? "+" : ""}</b>messages in the last hour</span>`,
    ];
    el.pulse.innerHTML = S.opened || S.msgs.size ? parts.join("") : "";
  }

  // ---------- feed ----------

  const feedQueue = [];
  const feedNodes = new Map(); // id -> li
  function queueFeed(m, live) { feedQueue.push([m, live]); }

  function tryParseJsonObj(body) {
    const s = String(body || "").trim();
    if (!s.startsWith("{")) return null;
    try {
      const obj = JSON.parse(s);
      return obj && typeof obj === "object" && !Array.isArray(obj) ? obj : null;
    } catch (_) {
      return null;
    }
  }

  function summarizeJsonObj(obj) {
    const entries = Object.entries(obj).filter(([k, v]) => v !== undefined && v !== null && v !== "" && !(k === "v" && Object.keys(obj).length > 1));
    if (!entries.length) return "{}";
    const parts = [];
    for (const [k, v] of entries) {
      let val;
      if (Array.isArray(v)) val = `[${v.length}]`;
      else if (typeof v === "object") val = "{...}";
      else val = String(v);
      parts.push(`${k}=${val}`);
      if (parts.join(" ").length > 140) break;
    }
    const out = parts.join(" ");
    return out.length > 160 ? out.slice(0, 159) + "…" : out;
  }

  function jsonSummary(kind, obj) {
    if (kind === "ALERT") {
      const sev = obj.severity ? `[${String(obj.severity).toUpperCase()}] ` : "";
      const detail = obj.text || [obj.code, obj.key].filter(Boolean).join("/") || summarizeJsonObj(obj);
      return `${sev}${detail}`;
    }
    if (kind === "FACT" && obj.tick_id !== undefined) {
      const mode = obj.mode || "plan";
      const n = Array.isArray(obj.actions) ? obj.actions.length : 0;
      return `${mode} tick #${obj.tick_id}: ${n} actions (applied: ${Boolean(obj.applied)})`;
    }
    return summarizeJsonObj(obj);
  }

  function renderBodyHtml(m) {
    if (!m.body) return "";
    const obj = tryParseJsonObj(m.body);
    if (!obj) return `<p class="m-body">${bodyHtml(m.body)}</p>`;
    const summary = jsonSummary(m.kind, obj);
    const pretty = JSON.stringify(obj, null, 2);
    return `<p class="m-body json-summary">${esc(summary)}</p><details class="json-details"><summary>JSON</summary><pre class="json-pretty">${esc(pretty)}</pre></details>`;
  }

  function bodyHtml(body) {
    const segs = String(body || "").split(" | ");
    return segs.map((s) => {
      let h = esc(s);
      h = h.replace(/^([a-z][a-z _]{0,18}):\s/i, '<span class="key">$1:</span> ');
      h = h.replace(/(^|[\s(,])([0-9a-f]{7,40})(?=$|[\s),.;:])/gi, (all, pre, hex) => /\d/.test(hex) && /[a-f]/i.test(hex) ? `${pre}<code>${hex}</code>` : all);
      h = h.replace(/(^|[\s(,])([A-Z][A-Z0-9_]{2,}=[^\s,)]+)/g, '$1<code>$2</code>');
      return `<span class="seg">${h}</span>`;
    }).join("");
  }

  function refsHtml(m) {
    const r = parseRefs(m.refs, m.to);
    const out = [];
    for (const b of r.branches) out.push(`<span class="chip" title="branch"><svg class="ic" aria-hidden="true"><use href="#i-branch"/></svg><span>${esc(b)}</span></span>`);
    for (const c of r.commits) out.push(`<span class="chip" title="commit ${esc(c)}"><svg class="ic" aria-hidden="true"><use href="#i-commit"/></svg><span>${esc(c.slice(0, 10))}</span></span>`);
    for (const id of r.ids) out.push(`<a class="chip" href="#m${id}" data-jump="${id}" title="message #${id}"><svg class="ic" aria-hidden="true"><use href="#i-link"/></svg><span>#${id}</span></a>`);
    for (const p of r.people) out.push(`<span class="chip chip-who ${hueOf(p)}"><span class="dot"></span><span>${esc(p)}</span></span>`);
    for (const o of r.other) out.push(`<span class="chip"><span>${esc(o)}</span></span>`);
    return { html: out.join(""), to: r.to };
  }

  function feedNode(m) {
    const li = document.createElement("li");
    const hub = m.role === "hub";
    li.className = "msg " + kindCls(m.kind) + (hub ? " msg-hub" : "");
    li.id = "m" + esc(m.id);
    li.dataset.id = String(m.id);
    const t = tsMs(m.ts);
    const refs = refsHtml(m);
    const ask = S.asks.get(m.id);
    let state = "";
    if (ask) { const st = askState(ask); state = `<span class="state ${st.cls}" data-ask="${m.id}">${esc(st.text)}</span>`; }
    const forTxt = Number.isSafeInteger(m._for) ? `<a class="m-id" href="#m${m._for}" data-jump="${m._for}">for #${m._for}</a>` : "";
    li.innerHTML = `<div class="m-head">
        <span class="m-kind">${esc(m.kind)}</span>
        <span class="m-from${hub ? "" : " " + hueOf(m.from)}">${hub ? "" : '<span class="dot"></span>'}${esc(m.from)}</span>
        ${m.topic ? `<button class="m-topic" type="button" data-topic="${esc(m.topic)}" title="Show only ${esc(m.topic)}">${esc(m.topic)}</button>` : ""}
        ${refs.to.length ? `<span class="m-to"><svg class="ic" aria-hidden="true"><use href="#i-arrow"/></svg>${refs.to.map(esc).join(", ")}</span>` : ""}
        ${state}${forTxt}
        <time class="m-time" datetime="${esc(m.ts)}" title="${esc(fullTime(t))}, #${esc(m.id)}">${esc(clock(t))}</time>
      </div>
      ${renderBodyHtml(m)}
      ${refs.html ? `<div class="m-refs">${refs.html}</div>` : ""}`;
    return li;
  }

  function flushFeed() {
    if (feedQueue.length) {
      const batch = feedQueue.splice(0).sort((a, b) => a[0].id - b[0].id);
      const anyLive = batch.some((x) => x[1]);
      for (const [m, live] of batch) {
        if (feedNodes.has(m.id)) continue;
        const li = feedNode(m);
        feedNodes.set(m.id, li);
        // newest first: find the first node with a smaller id
        let before = el.msgs.firstElementChild;
        while (before && +before.dataset.id > m.id) before = before.nextElementSibling;
        el.msgs.insertBefore(li, before);
        if (live && anyLive) { li.classList.add("arrive"); li.addEventListener("animationend", () => li.classList.remove("arrive"), { once: true }); }
      }
      while (feedNodes.size > FEED_MAX) {
        const last = el.msgs.lastElementChild;
        feedNodes.delete(+last.dataset.id); last.remove();
      }
      requestAnimationFrame(markClamped);
    }
    for (const id of dirtyAsk) {
      const a = S.asks.get(id); const node = feedNodes.get(id);
      if (!a || !node) continue;
      const st = askState(a);
      let badge = node.querySelector(`[data-ask="${id}"]`);
      if (!badge) {
        badge = document.createElement("span"); badge.dataset.ask = id;
        node.querySelector(".m-time").before(badge);
      }
      badge.className = "state " + st.cls; badge.textContent = st.text;
    }
    for (const id of dirtyAsk) {
      const a = S.asks.get(id);
      if (a && a.result && feedNodes.has(a.result.id) && !feedNodes.get(a.result.id).querySelector(".m-id")) {
        const n = feedNodes.get(a.result.id);
        if (Number.isSafeInteger(id)) n.querySelector(".m-time").insertAdjacentHTML("beforebegin", `<a class="m-id" href="#m${id}" data-jump="${id}">for #${id}</a>`);
      }
    }
    dirtyAsk.clear();
    applyFilters();
    renderKinds();
  }

  function markClamped() {
    for (const li of feedNodes.values()) {
      if (li.dataset.checked) continue;
      const b = li.querySelector(".m-body");
      li.dataset.checked = "1";
      if (b && b.scrollHeight > b.clientHeight + 2) {
        const btn = document.createElement("button");
        btn.type = "button"; btn.className = "m-more"; btn.textContent = "Show all";
        b.after(btn);
      }
    }
  }

  function topicMatch(topic, f) {
    if (!f) return true;
    if (f.endsWith("*")) return (topic || "").startsWith(f.slice(0, -1));
    return (topic || "").toLowerCase().includes(f.toLowerCase());
  }

  function applyFilters() {
    const f = S.filters;
    let shown = 0;
    for (const [id, li] of feedNodes) {
      const m = S.msgs.get(id);
      const ok = m && !f.off.has(m.kind) && (!f.from || m.from === f.from) && topicMatch(m.topic, f.topic);
      li.hidden = !ok;
      if (ok) shown++;
    }
    el.feedN.textContent = feedNodes.size ? (shown === feedNodes.size ? String(shown) : `${shown} of ${feedNodes.size}`) : "";
    const filtered = f.from || f.topic || [...f.off].some((k) => !HIDDEN_BY_DEFAULT.has(k)) || [...HIDDEN_BY_DEFAULT].some((k) => !f.off.has(k));
    el.fClear.hidden = !filtered;
    if (!feedNodes.size) {
      el.feedEmpty.hidden = false;
      el.feedEmpty.innerHTML = S.opened ? "The bus is quiet. Messages appear here the moment someone sends one." : "";
    } else if (!shown) {
      el.feedEmpty.hidden = false;
      el.feedEmpty.innerHTML = `Nothing matches these filters. <button class="m-more" type="button" data-clear="1">Clear the filters</button>`;
    } else {
      el.feedEmpty.hidden = true;
    }
  }

  function renderKinds() {
    const counts = {};
    for (const m of S.msgs.values()) counts[m.kind] = (counts[m.kind] || 0) + 1;
    // kinds that occur, plus any the viewer switched off, in protocol order
    const kinds = KINDS.concat(Object.keys(counts).filter((k) => !KINDS.includes(k)))
      .filter((k) => counts[k] || S.filters.off.has(k) && !HIDDEN_BY_DEFAULT.has(k));
    const html = kinds.map((k) => {
      const on = !S.filters.off.has(k);
      return `<button type="button" class="k-btn ${kindCls(k)}" aria-pressed="${on}" data-kind="${esc(k)}" title="${on ? "Hide" : "Show"} ${esc(k)}">${esc(k)}${counts[k] ? `<span class="n">${counts[k]}</span>` : ""}</button>`;
    }).join("");
    if (el.kinds.innerHTML !== html) el.kinds.innerHTML = html;
    const names = [...S.members.keys()].sort();
    if (S.msgs.size && [...S.msgs.values()].some((m) => m.from === "hub")) names.push("hub");
    const opts = `<option value="">Everyone</option>` + names.map((n) => `<option value="${esc(n)}"${n === S.filters.from ? " selected" : ""}>${esc(n)}</option>`).join("");
    if (el.fFrom.dataset.sig !== names.join(",") + "|" + S.filters.from) { el.fFrom.innerHTML = opts; el.fFrom.dataset.sig = names.join(",") + "|" + S.filters.from; }
    if (document.activeElement !== el.fTopic) el.fTopic.value = S.filters.topic;
  }

  // ---------- ages tick ----------

  function tick() {
    for (const t of document.querySelectorAll("time[data-ago]")) {
      const ms = +t.dataset.ago;
      t.textContent = t.dataset.short ? ago(ms) : agoText(ms);
    }
    // staleness and the hour count move with the clock
    renderStaleBanner();
    renderPeople(); renderPulse();
  }

  // ---------- connection ----------

  let ws = null, retryTimer = 0, pollTimer = 0, backoff = 1000, attempt = 0;

  function hubBase() {
    const saved = (load(HUB_KEY) || "").trim();
    if (saved) return saved.replace(/\/+$/, "");
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    return proto + "//" + location.host;
  }
  function httpBase() { return hubBase().replace(/^ws(s?):/, "http$1:"); }

  function setConn(state, text) {
    S.conn = state;
    el.conn.dataset.state = state;
    el.connText.textContent = text;
  }

  async function fetchState(token) {
    try {
      const r = await fetch(httpBase() + "/api/state", { headers: { Authorization: "Bearer " + token }, cache: "no-store", credentials: "omit" });
      if (r.ok) return await r.json();
      if (r.status === 401 || r.status === 400) {
        // The contract also names ?token=, for hubs that only read the query.
        const q = await fetch(httpBase() + "/api/state?token=" + encodeURIComponent(token), { cache: "no-store", credentials: "omit" });
        if (q.ok) return await q.json();
      }
    } catch (_) { /* no endpoint yet, or another origin without CORS: the stream alone is enough */ }
    return null;
  }

  // Tokens come and go: read the member list again and drop what lost its token.
  async function refreshRoster() {
    const token = load(TOKEN_KEY);
    if (!token) return;
    const st = await fetchState(token);
    if (!st || !Array.isArray(st.members)) return;
    S.roster = new Set(st.members.filter((x) => x && x.name).map((x) => x.name));
    for (const name of [...S.members.keys()]) if (!S.roster.has(name)) S.members.delete(name);
    for (const [k, a] of [...S.agents]) if (!S.roster.has(a.owner)) S.agents.delete(k);
    for (const x of st.members) if (x.role !== "observer" && x.role !== "hub" && !S.members.has(x.name)) { hueOf(x.name); member(x.name, x.role); }
    schedule();
  }
  setInterval(refreshRoster, ROSTER_SECS * 1000);

  async function start() {
    const token = load(TOKEN_KEY);
    if (!token) return showGate();
    el.gate.hidden = true; el.board.hidden = false; el.forget.hidden = false;
    schedule();
    if (!S.opened && !S.msgs.size) {
      setConn("connecting", "connecting");
      const st = await fetchState(token);
      if (st) { applySnapshot(st); }
    }
    connect();
  }

  function connect() {
    const token = load(TOKEN_KEY);
    if (!token) return showGate();
    clearTimeout(retryTimer);
    attempt++;
    const q = new URLSearchParams();
    if (S.lastId) q.set("since", String(S.lastId + 1)); else q.set("last", String(REPLAY));
    q.set("token", token);
    let sock;
    try { sock = new WebSocket(hubBase() + "/?" + q.toString()); }
    catch (_) { setConn("refused", "bad hub address"); return showGate("The hub address is not a valid ws:// or wss:// address."); }
    ws = sock;
    let opened = false;
    setConn(S.opened ? "retry" : "connecting", S.opened ? "reconnecting" : "connecting");
    sock.onopen = () => {
      opened = true; S.opened = true; S.failures = 0; backoff = 1000;
      setConn("live", "live");
      poll();
      clearInterval(pollTimer);
      pollTimer = setInterval(poll, POLL_SECS * 1000);
      schedule();
    };
    sock.onmessage = (e) => {
      S.lastFrameAt = Date.now();
      renderStaleBanner();
      let f; try { f = JSON.parse(e.data); } catch (_) { return; }
      if (f.reply) {
        if (f.ok && f.reply === "who") applyWho(f.connections);
        else if (f.ok && f.reply === "claims") applyClaims(f.claims);
        return;
      }
      ingest(f, S.opened);
    };
    sock.onclose = (e) => {
      if (ws !== sock) return;
      clearInterval(pollTimer);
      if (e.code === 1008) {
        setConn("refused", "token revoked");
        return showGate("The hub revoked this token. Ask the hub owner for a new observer token.");
      }
      if (!opened) {
        S.failures++;
        if (!S.opened && S.failures >= 2) {
          setConn("refused", "refused");
          showGate("The hub did not let this token in. It must be an observer token: member and orchestrator tokens are refused in a browser. If the token is right, the hub may be down.", true);
        }
      }
      const wait = e.code === 1012 || e.code === 1013 ? 1000 : backoff;
      backoff = Math.min(backoff * 2, 30000);
      if (S.conn !== "refused" || S.opened) setConn("retry", `reconnecting in ${Math.round(wait / 1000)} s`);
      retryTimer = setTimeout(connect, wait);
    };
    sock.onerror = () => { /* onclose follows */ };
  }

  function poll() {
    if (!ws || ws.readyState !== 1) return;
    ws.send(JSON.stringify({ op: "who", rid: "who" }));
    ws.send(JSON.stringify({ op: "claims", rid: "claims" }));
  }

  function showGate(err, keepTrying) {
    el.gate.hidden = false;
    if (!S.opened) el.board.hidden = true;
    el.forget.hidden = !load(TOKEN_KEY);
    el.gateErr.hidden = !err;
    el.gateErr.textContent = err || "";
    el.gateHub.value = load(HUB_KEY) || "";
    if (!keepTrying) { clearTimeout(retryTimer); if (ws) { const w = ws; ws = null; try { w.close(); } catch (_) { /* closed */ } } }
    if (!err) setConn("idle", "not connected");
    el.gateToken.focus();
  }

  function forget() {
    store(TOKEN_KEY, null);
    clearTimeout(retryTimer); clearInterval(pollTimer);
    if (ws) { const w = ws; ws = null; try { w.close(); } catch (_) { /* closed */ } }
    location.reload();
  }

  // ---------- events ----------

  el.gateForm.addEventListener("submit", (e) => {
    e.preventDefault();
    const t = el.gateToken.value.trim();
    if (!t) return;
    const h = el.gateHub.value.trim();
    if (h && !/^wss?:\/\//i.test(h)) {
      el.gateErr.hidden = false; el.gateErr.textContent = "The hub address starts with ws:// or wss://.";
      return;
    }
    store(HUB_KEY, h || null);
    store(TOKEN_KEY, t);
    el.gateToken.value = "";
    clearTimeout(retryTimer);
    if (ws) { const w = ws; ws = null; try { w.close(); } catch (_) { /* closed */ } }
    S.failures = 0; backoff = 1000;
    if (el.wsTabs) {
    el.wsTabs.addEventListener("click", (e) => {
      const btn = e.target.closest(".ws-tab[data-tab]");
      if (btn) setTab(btn.dataset.tab);
    });
  }
  document.addEventListener("click", (e) => {
    const jt = e.target.closest("[data-jump-tab]");
    if (jt) setTab(jt.dataset.jumpTab);
  });
  setTab(load(TAB_KEY) || "overview");
  initTheme();

  start();
  });
  el.forget.addEventListener("click", forget);

  el.kinds.addEventListener("click", (e) => {
    const b = e.target.closest("[data-kind]"); if (!b) return;
    const k = b.dataset.kind;
    if (e.altKey || e.metaKey) {
      // alt click: show only this kind
      S.filters.off = new Set(KINDS.filter((x) => x !== k));
    } else if (S.filters.off.has(k)) S.filters.off.delete(k); else S.filters.off.add(k);
    saveFilters(); applyFilters(); renderKinds();
  });
  el.fFrom.addEventListener("change", () => { S.filters.from = el.fFrom.value; saveFilters(); applyFilters(); });
  el.fTopic.addEventListener("input", () => { S.filters.topic = el.fTopic.value.trim(); saveFilters(); applyFilters(); });
  function clearFilters() {
    S.filters = { off: new Set(HIDDEN_BY_DEFAULT), from: "", topic: "" };
    saveFilters(); el.fTopic.value = ""; applyFilters(); renderKinds();
  }
  el.fClear.addEventListener("click", clearFilters);

  document.addEventListener("click", (e) => {
    const topic = e.target.closest("[data-topic]");
    if (topic) { S.filters.topic = topic.dataset.topic; el.fTopic.value = S.filters.topic; saveFilters(); applyFilters(); $("feed").scrollIntoView({ behavior: "smooth", block: "start" }); return; }
    if (e.target.closest("[data-clear]")) { clearFilters(); return; }
    const jump = e.target.closest("[data-jump]");
    if (jump) {
      e.preventDefault();
      const n = feedNodes.get(+jump.dataset.jump);
      if (n) {
        if (n.hidden) clearFilters();
        n.scrollIntoView({ behavior: "smooth", block: "center" });
        n.classList.remove("flash"); void n.offsetWidth; n.classList.add("flash");
      }
      return;
    }
    const more = e.target.closest(".m-more");
    if (more && !more.dataset.clear) {
      const li = more.closest(".msg"); li.classList.toggle("open");
      more.textContent = li.classList.contains("open") ? "Show less" : "Show all";
      return;
    }
    const ag = e.target.closest(".agent");
    if (ag && !e.target.closest("a")) toggleAgent(ag);
  });
  document.addEventListener("keydown", (e) => {
    const ag = e.target.closest && e.target.closest(".agent");
    if (ag && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); toggleAgent(ag); }
  });
  function toggleAgent(ag) {
    const k = ag.dataset.key;
    if (openAgents.has(k)) openAgents.delete(k); else openAgents.add(k);
    ag.classList.toggle("open", openAgents.has(k));
  }

  function renderTriage() {
    if (!el.triage) return;
    const items = [];
    for (const a of S.agents.values()) {
      const m = S.members.get(a.owner);
      if (!m) continue;
      const st = agentState(a, m);
      if (st === "blocked" || st === "suspect" || st === "starved") {
        items.push({ cls: "", tab: "people", text: `${st}: ${a.owner}/${a.agent}` });
      }
    }
    const snap = S.snapshot || {};
    for (const p of Array.isArray(snap.pools) ? snap.pools : []) {
      const pst = String(p.state || "").toLowerCase();
      if (pst === "drain" || pst === "throttled" || pst === "closed") {
        items.push({ cls: "is-warn", tab: "pools", text: `pool ${p.pool}: ${pst}` });
      }
    }
    const checks = snap.regress && Array.isArray(snap.regress.checks) ? snap.regress.checks : [];
    for (const c of checks) {
      if (c && c.status === "regression_confirmed") {
        items.push({ cls: "", tab: "metrics", text: `regression: ${c.metric || "metric"} (+${Number(c.worse_pct || 0).toFixed(1)}%)` });
      }
    }
    if (!items.length) {
      el.triage.hidden = true;
      el.triage.innerHTML = "";
      return;
    }
    el.triage.hidden = false;
    el.triage.innerHTML = `<span class="triage-lbl"><svg class="ic" aria-hidden="true"><use href="#i-alert"/></svg>Triage</span>` +
      items.slice(0, 6).map((it) => `<button type="button" class="triage-item ${it.cls}" data-jump-tab="${esc(it.tab)}">${esc(it.text)}</button>`).join("") +
      (items.length > 6 ? `<span class="count">+${items.length - 6} more</span>` : "");
  }

  function updateTabCounts() {
    if (!el.wsTabs) return;
    const snap = S.snapshot || {};
    const setBadge = (id, count, alert) => {
      const n = $(id);
      if (n) n.textContent = count > 0 ? String(count) : "";
      const btn = n ? n.closest(".ws-tab") : null;
      if (btn) btn.classList.toggle("has-alert", Boolean(alert));
    };
    let peopleAlert = false;
    for (const a of S.agents.values()) {
      const m = S.members.get(a.owner);
      if (!m) continue;
      const st = agentState(a, m);
      if (st === "blocked" || st === "suspect" || st === "starved") { peopleAlert = true; break; }
    }
    const tasks = Array.isArray(snap.tasks) ? snap.tasks : [];
    const pools = Array.isArray(snap.pools) ? snap.pools : [];
    const landing = Array.isArray(snap.landing) ? snap.landing : [];
    const runs = Array.isArray(snap.runs) ? snap.runs : [];
    const branches = Array.isArray(snap.branches) ? snap.branches : [];
    const checks = snap.regress && Array.isArray(snap.regress.checks) ? snap.regress.checks : [];
    const claimsCount = [...S.claims.values()].filter((c) => !S.roster || S.roster.has(c.holder)).length;
    const openRunsCount = [...S.asks.values()].filter((a) => a.msg.kind === "RUN_REQUEST" && isOpenRun(a)).length;

    setBadge("tn-people", S.members.size, peopleAlert);
    setBadge("tn-tasks", tasks.length, tasks.some((t) => t && t.state === "stuck"));
    setBadge("tn-pools", pools.length, pools.some((p) => p && (p.state === "drain" || p.state === "throttled" || p.state === "closed")));
    setBadge("tn-landing", landing.length, landing.some((l) => l && (l.state === "failed" || l.state === "conflict")));
    setBadge("tn-runs", runs.length, runs.some((r) => r && (r.outcome === "fail" || r.outcome === "error")));
    setBadge("tn-branches", branches.length, branches.some((b) => b && b.state === "orphan"));
    setBadge("tn-metrics", checks.length, checks.some((c) => c && c.status === "regression_confirmed"));
    setBadge("tn-ledger", claimsCount + openRunsCount, false);
  }

  function setTab(tab) {
    const next = VALID_TABS.has(tab) ? tab : "overview";
    if (el.board) el.board.dataset.tab = next;
    store(TAB_KEY, next);
    if (el.wsTabs) {
      for (const btn of el.wsTabs.querySelectorAll(".ws-tab")) {
        btn.setAttribute("aria-selected", String(btn.dataset.tab === next));
      }
    }
  }

  function applyTheme(mode) {
    const m = mode === "dark" || mode === "light" ? mode : "system";
    if (m === "system") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = m;
    if (!el.themeToggle) return;
    const icon = m === "dark" ? "#i-moon" : m === "light" ? "#i-sun" : "#i-monitor";
    const lbl = m === "dark" ? "Dark" : m === "light" ? "Light" : "Auto";
    const use = el.themeToggle.querySelector("use");
    const span = el.themeToggle.querySelector(".theme-lbl");
    if (use) use.setAttribute("href", icon);
    if (span) span.textContent = lbl;
    el.themeToggle.title = `Theme: ${m}`;
  }

  function initTheme() {
    applyTheme(load(THEME_KEY) || "system");
    if (el.themeToggle) {
      el.themeToggle.addEventListener("click", () => {
        const cur = load(THEME_KEY) || "system";
        const next = cur === "system" ? "dark" : cur === "dark" ? "light" : "system";
        store(THEME_KEY, next === "system" ? null : next);
        applyTheme(next);
      });
    }
  }

  // Keep sticky offsets right when the bar wraps. On a phone only the section
  // switcher stays on screen: the name, numbers and connection row scroll away.
  const phone = matchMedia("(max-width: 640px)");
  const sizeBar = () => {
    const inner = el.bar.querySelector(".bar-in");
    const lift = phone.matches ? inner.offsetHeight : 0;
    el.bar.style.top = -lift + "px";
    document.documentElement.style.setProperty("--bar-h", (el.bar.offsetHeight - lift) + "px");
  };
  phone.addEventListener("change", sizeBar);
  if ("ResizeObserver" in window) new ResizeObserver(sizeBar).observe(el.bar); else addEventListener("resize", sizeBar);
  sizeBar();

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      tick();
      if (load(TOKEN_KEY) && (!ws || ws.readyState > 1)) { backoff = 1000; connect(); }
    }
  });
  setInterval(tick, 15000);

  if (el.wsTabs) {
    el.wsTabs.addEventListener("click", (e) => {
      const btn = e.target.closest(".ws-tab[data-tab]");
      if (btn) setTab(btn.dataset.tab);
    });
  }
  document.addEventListener("click", (e) => {
    const jt = e.target.closest("[data-jump-tab]");
    if (jt) setTab(jt.dataset.jumpTab);
  });
  setTab(load(TAB_KEY) || "overview");
  initTheme();

  start();
})();
