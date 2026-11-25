"use strict";

// Run with: node bus/tests/dashboard_render_test.js
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");
const assert = require("node:assert/strict");

class Element {
  constructor() {
    this.innerHTML = "";
    this.dataset = {};
    this.style = {};
    this.classList = { add() {}, remove() {}, toggle() {} };
  }
  addEventListener() {}
  querySelector() { return new Element(); }
  insertBefore() {}
}

const elements = new Map();
const document = {
  getElementById(id) {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  },
  createElement() { return new Element(); },
  activeElement: null,
};
const appPath = path.join(__dirname, "../ui/app.js");
let source = fs.readFileSync(appPath, "utf8");
const eventSection = "  // ---------- events ----------";
assert(source.includes(eventSection), "could not find dashboard event section");
source = source.slice(0, source.indexOf(eventSection)) + `
  globalThis.dashboardTest = { ingest, feedNode, renderKinds, S, present, presenceHtml, agentState, applySnapshot, applyWho, member };
})();
`;

const context = {
  document,
  localStorage: { getItem() { return null; }, setItem() {}, removeItem() {} },
  requestAnimationFrame() { return 1; },
  setInterval() { return 1; },
  clearInterval() {},
  clearTimeout() {},
  Date,
  Map,
  Set,
  URLSearchParams,
  console,
};
vm.runInNewContext(source, context, { filename: appPath });

const hostile = "<img src=x onerror=alert(1)>";
const test = context.dashboardTest;
const messages = [
  { id: 1, kind: "TASK", role: "member", from: hostile, topic: hostile, body: hostile, ts: "2026-11-25T12:00:00Z", refs: [], to: [] },
  { id: 2, kind: hostile, role: "member", from: "worker", topic: "", body: "kind check", ts: "2026-11-25T12:00:01Z", refs: [], to: [] },
  { id: "<img src=x onerror=alert(1)>", kind: "CHAT", role: "hub", from: "hub", topic: "", body: hostile, ts: "2026-11-25T12:00:02Z", refs: [], to: [] },
];
for (const message of messages) test.ingest(message, false);

const feedHtml = messages.slice(0, 2).map((message) => test.feedNode(message).innerHTML).join("\n");
test.renderKinds();
const filterHtml = document.getElementById("f-from").innerHTML;
const kindHtml = document.getElementById("kinds").innerHTML;
const combined = `${feedHtml}\n${filterHtml}\n${kindHtml}`;
assert(combined.includes("&lt;img"), "hostile text should be escaped in rendered output");
assert(!combined.includes("<img"), "rendered output must not contain a live img element");
assert.equal(test.S.msgs.has(messages[2].id), false, "string message IDs must be rejected");

const offer = (id, topic, to = "worker") => test.ingest({
  id, kind: "TASK", role: "member", from: "asker", topic, body: "offer",
  ts: "2026-11-25T12:01:00Z", refs: [`to:${to}`], to: [to],
}, false);
for (let id = 10; id <= 14; id++) offer(id, `topic-${id}`);
const answer = (id, kind, topic, refs = []) => test.ingest({
  id, kind, role: "member", from: "worker", topic, body: "answer",
  ts: "2026-11-25T12:02:00Z", refs, to: [],
}, false);
answer(20, "ACCEPT", "", ["#10"]);
answer(21, "DECLINE", "", ["#11"]);
answer(22, "READY", "", ["#12"]);
answer(23, "RUN_RESULT", "", ["#13"]);
answer(24, "ACCEPT", "topic-14");
const states = [10, 11, 12, 13, 14].map((id) => {
  const ask = test.S.asks.get(id);
  return [ask.accepted.length, ask.declined.length, ask.result && ask.result.id,
    ask.replies.map((reply) => reply.id)];
});
// This is the pre-index matching predicate, applied to the same synthetic messages.
const previousStates = [10, 11, 12, 13, 14].map((id) => {
  const ask = test.S.asks.get(id);
  const message = ask.msg;
  const recipients = (message.refs || []).filter((ref) => /^to:/i.test(ref))
    .map((ref) => ref.slice(3).toLowerCase());
  const replies = test.S.replies.filter((reply) => reply.id > message.id && reply.from !== message.from
    && (reply.ids.includes(message.id) || (message.topic && reply.topic === message.topic
      && recipients.includes(reply.from)
      && !((reply.kind === "ACCEPT" || reply.kind === "DECLINE")
        && reply.ids.some((replyId) => test.S.asks.has(replyId))))));
  const deliveries = replies.filter((reply) => ["READY", "RUN_RESULT", "FIX", "STUCK", "RELEASE", "LANDED"].includes(reply.kind));
  const result = deliveries.find((reply) => reply.kind === "RUN_RESULT") || deliveries[0] || null;
  return [replies.filter((reply) => reply.kind === "ACCEPT").length,
    replies.filter((reply) => reply.kind === "DECLINE").length, result && result.id,
    replies.map((reply) => reply.id)];
});
assert.deepEqual(JSON.parse(JSON.stringify(states)), JSON.parse(JSON.stringify(previousStates)));
assert.deepEqual(JSON.parse(JSON.stringify(states)), [
  [1, 0, null, [20]],
  [0, 1, null, [21]],
  [0, 0, 22, [22]],
  [0, 0, 23, [23]],
  [1, 0, null, [24]],
]);
console.log("dashboard hostile render test passed");

// ---------- presence: only real signs of life ----------
const iso = (minAgo) => new Date(Date.now() - minAgo * 60000).toISOString();
const row = (owner, agent, text, minAgo, changedMinAgo, state = "running") => ({
  owner, agent, text, state, model: "", branch: "", ts: iso(minAgo), id: 1,
  ...(changedMinAgo === undefined ? {} : { changed: iso(changedMinAgo) }),
});
const snap = (agents) => test.applySnapshot({ hub: { id: 1000, now: new Date().toISOString() }, agents, claims: [], recent: [] });
const names = ["beat", "talker", "changer", "linked", "oldmsg"];
for (const n of names) test.member(n, "member");
// A heartbeat: the row is fresh, its text last changed 7 hours ago.
// The hub sends `changed`. A participant that sends a real message 5 minutes ago. A row changed 3 minutes ago.
snap([
  row("beat", "main", "waiting for work", 1, 420, "idle"),
  row("beat", "w1", "building", 1, 420),
  row("changer", "main", "now compiling", 2, 3),
]);
test.ingest({ id: 500, kind: "CHAT", role: "member", from: "talker", topic: "", body: "hi", ts: iso(5), refs: [], to: [] }, false);
test.ingest({ id: 501, kind: "CHAT", role: "member", from: "oldmsg", topic: "", body: "hi", ts: iso(7 * 60), refs: [], to: [] }, false);
// A STATUS message is never a real message.
test.ingest({ id: 502, kind: "STATUS", role: "member", from: "beat", topic: "agent/main", body: "waiting for work", ts: iso(0), refs: ["state:idle"], to: [] }, true);
test.applyWho([{ name: "linked", role: "member", since: iso(60) }]);
const m = (n) => test.S.members.get(n);
assert.equal(test.present(m("beat")), false, "a heartbeat only participant is offline");
assert.equal(m("beat").lastMsg, 0, "a STATUS is not a message");
assert.equal(test.present(m("talker")), true, "a real message 5 min ago is a sign of life");
assert.equal(test.present(m("changer")), true, "a changed row 3 min ago is a sign of life");
assert.equal(test.present(m("linked")), true, "a connected participant is online");
assert.equal(test.present(m("oldmsg")), false, "a message 7 h ago is no sign of life");
const beatHtml = test.presenceHtml(m("beat"));
assert(/offline, last status change/.test(beatHtml), beatHtml);
assert(/last message/.test(test.presenceHtml(m("oldmsg"))));
assert(/active, last message/.test(test.presenceHtml(m("talker"))));
assert(/active, last status change/.test(test.presenceHtml(m("changer"))));
assert(/online since/.test(test.presenceHtml(m("linked"))));
const beatRow = test.S.agents.get("beat\u0000w1");
assert.equal(test.agentState(beatRow, m("beat")), "stale", "workers of an offline participant are grey");
assert.equal(beatRow.text, "building", "their text stays");
assert.equal(test.agentState(test.S.agents.get("changer\u0000main"), m("changer")), "running");
//Without `changed` from the hub: a fresh page sees no change, a later different text is one.
test.S.agents.clear();
snap([row("beat", "main", "waiting for work", 1, undefined, "idle")]);
assert.equal(test.present(m("beat")), false, "no history means no change");
snap([row("beat", "main", "waiting for work", 0, undefined, "idle")]);
assert.equal(test.present(m("beat")), false, "same text again is no change");
test.ingest({ id: 503, kind: "STATUS", role: "member", from: "beat", topic: "agent/main", body: "reading the log", ts: iso(0), refs: ["state:idle"], to: [] }, true);
assert.equal(test.present(m("beat")), true, "a different text is a change");

// ---------- derived state and stale banner ----------
test.applySnapshot({
  hub: { id: 1005, now: new Date().toISOString() },
  as_of: Math.floor(Date.now() / 1000),
  derived: {
    as_of: Math.floor(Date.now() / 1000),
    workers: [
      { owner: "talker", agent: "w-starved", class: "codex", state: "starved", reported_state: "idle", mode: "auto", age_secs: 15, idle_secs: 15, reason: "scope issue/10 is claimed", model: "codex", branch: "", text: "waiting", as_of: Math.floor(Date.now() / 1000) },
      { owner: "talker", agent: "w-suspect", class: "codex", state: "suspect", reported_state: "running", mode: "auto", age_secs: 140, idle_secs: 0, reason: "heartbeat late by 140 s", model: "codex", branch: "codex/fix", text: "building", as_of: Math.floor(Date.now() / 1000) },
    ],
    slots: [],
    idle: [],
    alerts: [],
    counts: { starved: 1, suspect: 1 },
  },
  agents: [
    row("talker", "w-starved", "waiting", 0, 0, "idle"),
    row("talker", "w-suspect", "building", 0, 0, "running"),
  ],
  claims: [],
  recent: [],
});
assert.equal(test.agentState(test.S.agents.get("talker\u0000w-starved"), m("talker")), "starved");
assert.equal(test.agentState(test.S.agents.get("talker\u0000w-suspect"), m("talker")), "suspect");
assert.equal(test.S.staleBanner, false);
test.S.lastFrameAt = Date.now() - 95000;
test.applyWho([{ name: "talker", role: "member", since: iso(10) }]);
assert.equal(Date.now() - test.S.lastFrameAt >= 90000, true, "stale after 90s without a frame");
console.log("dashboard presence test passed");

// ---------- FACT hidden by default and JSON body formatting ----------
assert.equal(test.S.filters.off.has("FACT"), true, "FACT must be hidden by default");
assert.equal(test.S.filters.off.has("STATUS"), true, "STATUS must remain hidden by default");

const alertMsg = {
  id: 601, kind: "ALERT", role: "hub", from: "hub", topic: "alert/IDLE/box1/codex/1",
  body: JSON.stringify({ severity: "warn", code: "IDLE", key: "box1/codex/1", text: "slot idle for 600s" }),
  ts: "2026-11-25T12:10:00Z", refs: [], to: [],
};
const factDecisionMsg = {
  id: 602, kind: "FACT", role: "hub", from: "hub", topic: "decision/42",
  body: JSON.stringify({ v: 1, tick_id: 42, mode: "enforce", applied: true, actions: [{ action: "lease" }, { action: "expire" }] }),
  ts: "2026-11-25T12:10:01Z", refs: [], to: [],
};
const taskUpdateMsg = {
  id: 603, kind: "TASK_UPDATE", role: "hub", from: "hub", topic: "task/t1",
  body: JSON.stringify({ v: 1, task: "t1", state: "leased", by: "hub" }),
  ts: "2026-11-25T12:10:02Z", refs: [], to: [],
};
const hostileJsonMsg = {
  id: 604, kind: "ALERT", role: "hub", from: "hub", topic: "alert/XSS",
  body: JSON.stringify({ severity: "error", text: hostile }),
  ts: "2026-11-25T12:10:03Z", refs: [], to: [],
};
const plainChatMsg = {
  id: 605, kind: "CHAT", role: "member", from: "talker", topic: "",
  body: "plain text message",
  ts: "2026-11-25T12:10:04Z", refs: [], to: [],
};

const alertHtml = test.feedNode(alertMsg).innerHTML;
assert(alertHtml.includes("[WARN] slot idle for 600s"), `unexpected ALERT summary: ${alertHtml}`);
assert(alertHtml.includes('<details class="json-details"><summary>JSON</summary><pre class="json-pretty">'), "ALERT should render collapsible JSON");
assert(alertHtml.includes('&quot;severity&quot;: &quot;warn&quot;'), "ALERT should render pretty-printed JSON");

const factHtml = test.feedNode(factDecisionMsg).innerHTML;
assert(factHtml.includes("enforce tick #42: 2 actions (applied: true)"), `unexpected FACT summary: ${factHtml}`);
assert(factHtml.includes('<details class="json-details"><summary>JSON</summary><pre class="json-pretty">'), "FACT should render collapsible JSON");
assert(factHtml.includes('&quot;tick_id&quot;: 42'), "FACT should render pretty-printed JSON");

const taskUpdateHtml = test.feedNode(taskUpdateMsg).innerHTML;
assert(taskUpdateHtml.includes("task=t1 state=leased by=hub"), `unexpected TASK_UPDATE summary: ${taskUpdateHtml}`);
assert(taskUpdateHtml.includes('<details class="json-details"><summary>JSON</summary><pre class="json-pretty">'), "TASK_UPDATE should render collapsible JSON");

const hostileJsonHtml = test.feedNode(hostileJsonMsg).innerHTML;
assert(hostileJsonHtml.includes("&lt;img"), "hostile JSON text must be escaped");
assert(!hostileJsonHtml.includes("<img"), "hostile JSON must not contain a live img element");

const plainHtml = test.feedNode(plainChatMsg).innerHTML;
assert(plainHtml.includes("plain text message"), "plain text message must render");
assert(!plainHtml.includes("json-details"), "plain text message must not render json-details");
console.log("dashboard json format test passed");
