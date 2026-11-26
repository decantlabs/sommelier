import { inferProvider } from "../components/ProviderMark";
import type {
  AccessRole,
  AgentMachine,
  ApiStateSnapshot,
  BatchItem,
  BusMessage,
  ClaimItem,
  Contributor,
  FactoryAlert,
  FactorySnapshot,
  FeatureItem,
  LandingItem,
  MatrixCell,
  QuotaPool,
  RegressionCheck,
  ReviewItem,
  RoleLease,
  RunOrderItem,
  SchedulerStatus,
  StampEntry,
  SubmissionItem,
  SubmissionState,
  TaskState,
  WorkAttempt,
  WorkTask,
} from "./types";

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function asArray(value: unknown): Record<string, unknown>[] {
  if (!Array.isArray(value)) return [];
  return value.map(asRecord).filter((item): item is Record<string, unknown> => Boolean(item));
}

function str(obj: Record<string, unknown> | undefined, key: string, fallback = ""): string {
  const val = obj?.[key];
  return typeof val === "string" ? val : fallback;
}

function num(obj: Record<string, unknown> | undefined, key: string, fallback = 0): number {
  const val = obj?.[key];
  return typeof val === "number" && Number.isFinite(val) ? val : fallback;
}

function bool(obj: Record<string, unknown> | undefined, key: string, fallback = false): boolean {
  const val = obj?.[key];
  return typeof val === "boolean" ? val : fallback;
}

function strMap(obj: Record<string, unknown> | undefined): Record<string, number> | undefined {
  if (!obj) return undefined;
  const out: Record<string, number> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (typeof v === "number" && Number.isFinite(v)) {
      out[k] = v;
    }
  }
  return Object.keys(out).length > 0 ? out : undefined;
}

function formatClock(raw: unknown): string {
  if (typeof raw === "number" && raw > 0) {
    const ms = raw > 1e12 ? raw : raw * 1000;
    return new Date(ms).toLocaleTimeString("en-GB", { hour12: false });
  }
  if (typeof raw === "string") {
    const tIdx = raw.indexOf("T");
    if (tIdx !== -1 && raw.length >= tIdx + 9) {
      return raw.slice(tIdx + 1, tIdx + 9);
    }
    return raw;
  }
  return "—";
}

function formatRelativeAge(tsSecOrMs: number, nowSec: number): string {
  if (!tsSecOrMs) return "just now";
  const sec = tsSecOrMs > 1e12 ? Math.floor(tsSecOrMs / 1000) : tsSecOrMs;
  const diff = Math.max(0, nowSec - sec);
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  return `${Math.floor(diff / 3600)}h ago`;
}

function titleCaseTaskState(raw: string): TaskState {
  const normalized = raw.trim().toLowerCase();
  const match: Record<string, TaskState> = {
    draft: "Draft",
    ready: "Ready",
    offered: "Offered",
    leased: "Leased",
    running: "Running",
    delivered: "Delivered",
    landing: "Landing",
    closed: "Closed",
    stuck: "Stuck",
    cancelled: "Cancelled",
  };
  return match[normalized] ?? "Ready";
}

function titleCaseRole(raw: string): "Orchestrator" | "Member" | "Observer" {
  switch (raw.toLowerCase()) {
    case "orchestrator":
      return "Orchestrator";
    case "observer":
      return "Observer";
    default:
      return "Member";
  }
}

function capitalize(raw: string): string {
  if (!raw) return "";
  return raw.charAt(0).toUpperCase() + raw.slice(1);
}

function initialsOf(name: string): string {
  const clean = name.replace(/^[^a-zA-Z0-9]+/, "");
  if (clean.length >= 2) return clean.slice(0, 2).toUpperCase();
  return (clean || "SM").toUpperCase();
}

function scopesRoughlyOverlap(a: string, b: string): boolean {
  if (!a || !b) return false;
  const partsA = a
    .split(",")
    .map((s) => s.trim().replace(/\*+$/g, ""))
    .filter(Boolean);
  const partsB = b
    .split(",")
    .map((s) => s.trim().replace(/\*+$/g, ""))
    .filter(Boolean);
  return partsA.some((pa) => partsB.some((pb) => pa.startsWith(pb) || pb.startsWith(pa)));
}

function parseFindings(raw: unknown): string | undefined {
  if (typeof raw !== "string" || !raw.trim()) return undefined;
  const trimmed = raw.trim();
  if (trimmed.startsWith("[")) {
    try {
      const arr = JSON.parse(trimmed) as unknown[];
      if (Array.isArray(arr)) {
        if (arr.length === 0) return "0 findings";
        return `${arr.length} ${arr.length === 1 ? "finding" : "findings"}`;
      }
    } catch {
      return trimmed;
    }
  }
  return trimmed;
}

function layoutTasks(rawTasks: Omit<WorkTask, "x" | "y">[]): WorkTask[] {
  const levelMap = new Map<string, number>();
  const byId = new Map(rawTasks.map((t) => [t.id, t]));

  function getDepth(id: string, stack = new Set<string>()): number {
    const cached = levelMap.get(id);
    if (cached !== undefined) return cached;
    if (stack.has(id)) return 0;
    const task = byId.get(id);
    if (!task || (!task.deps?.length && !task.conflict)) {
      levelMap.set(id, 0);
      return 0;
    }
    stack.add(id);
    const parentIds = [...(task.deps ?? []), ...(task.conflict ? [task.conflict] : [])];
    const maxParent = parentIds.reduce((acc, pid) => {
      if (!byId.has(pid)) return acc;
      return Math.max(acc, getDepth(pid, stack) + 1);
    }, 0);
    stack.delete(id);
    levelMap.set(id, maxParent);
    return maxParent;
  }

  for (const t of rawTasks) getDepth(t.id);

  const columns = new Map<number, Omit<WorkTask, "x" | "y">[]>();
  for (const t of rawTasks) {
    const col = levelMap.get(t.id) ?? 0;
    const list = columns.get(col) ?? [];
    list.push(t);
    columns.set(col, list);
  }

  const out: WorkTask[] = [];
  for (const [col, list] of columns.entries()) {
    list.forEach((task, rowIdx) => {
      out.push({
        ...task,
        x: 105 + col * 465,
        y: 137 + rowIdx * 295,
      });
    });
  }
  return out;
}

function adaptAgentMachines(raw: ApiStateSnapshot, nowSec: number): AgentMachine[] {
  const store = asRecord(raw.store);
  const workers = asArray(asRecord(raw.derived)?.workers);
  const machines = new Map<string, AgentMachine>();
  const agentRows = new Map<string, Record<string, unknown>>();
  const workerRows = new Map<string, Record<string, unknown>>();
  const leases = new Map<string, Record<string, unknown>>();
  const key = (owner: string, name?: string) => JSON.stringify([owner, name ?? ""]);
  const machine = (owner: string, name?: string) => {
    const id = key(owner, name);
    let entry = machines.get(id);
    if (!entry) {
      entry = { owner, name, agents: [] };
      machines.set(id, entry);
    }
    return entry;
  };

  for (const row of asArray(raw.machines ?? store?.machines)) {
    const owner = str(row, "owner");
    const name = str(row, "machine");
    if (owner && name) machine(owner, name);
  }

  const declarations = new Map<string, Record<string, unknown>>();
  for (const row of asArray(raw.capacities ?? store?.capacities)) {
    const owner = str(row, "owner");
    const name = str(row, "machine");
    if (!owner || !name) continue;
    declarations.set(JSON.stringify([owner, name, str(row, "class")]), row);
  }
  for (const row of declarations.values()) {
    const slots = row.useful_parallel;
    if (typeof slots !== "number" || !Number.isFinite(slots) || slots < 0) continue;
    const entry = machine(str(row, "owner"), str(row, "machine"));
    entry.slots = (entry.slots ?? 0) + slots;
  }

  // The roster and the store may both describe one process. Preserve its store metadata.
  for (const row of [...asArray(store?.agents), ...asArray(raw.agents)]) {
    const owner = str(row, "owner");
    const name = str(row, "agent") || str(row, "name");
    if (owner && name) {
      const id = key(owner, name);
      agentRows.set(id, { ...agentRows.get(id), ...row });
    }
  }

  for (const row of workers) {
    const owner = str(row, "owner");
    const name = str(row, "agent");
    if (owner && name) workerRows.set(key(owner, name), row);
  }

  for (const row of asArray(raw.leases)) {
    const owner = str(row, "owner");
    const name = str(row, "worker");
    const state = str(row, "state");
    if (!owner || !name || (state !== "active" && state !== "suspect")) continue;
    const id = key(owner, name);
    if (num(row, "granted_at") >= num(leases.get(id), "granted_at")) leases.set(id, row);
  }

  for (const id of new Set([...agentRows.keys(), ...workerRows.keys()])) {
    const row = agentRows.get(id);
    const worker = workerRows.get(id);
    const lease = leases.get(id);
    const owner = str(row, "owner") || str(worker, "owner");
    const name = str(row, "agent") || str(row, "name") || str(worker, "agent");
    const host = str(lease, "machine") || str(row, "machine") || undefined;
    const model = str(worker, "model") || str(row, "model") || undefined;
    const agentClass = str(worker, "class") || str(row, "class") || str(lease, "class") || undefined;
    const mode = str(worker, "mode");
    const reported = str(row, "reported_state") || str(row, "state", "unknown");
    const state = str(worker, "state") || (bool(row, "stale") ? "stale" : reported);
    const statusAt = num(row, "as_of") || num(worker, "as_of");

    machine(owner, host).agents.push({
      name,
      state: capitalize(state.replaceAll("_", " ")),
      provider: inferProvider(model, agentClass),
      model,
      agentClass,
      mode: mode === "auto" ? "Auto" : mode === "own" ? "Own" : undefined,
      taskId: str(lease, "task_id") || str(row, "task_id") || undefined,
      branch: str(worker, "branch") || str(row, "branch") || undefined,
      lastStatus: typeof worker?.age_secs === "number" && worker.age_secs >= 0
        ? formatRelativeAge(nowSec - worker.age_secs, nowSec)
        : statusAt > 0 ? formatRelativeAge(statusAt, nowSec) : undefined,
      statusDetail: str(worker, "text") || str(row, "text") || undefined,
      reason: str(worker, "reason") || undefined,
    });
  }

  return [...machines.values()].sort((a, b) =>
    a.owner.localeCompare(b.owner) || (a.name ?? "").localeCompare(b.name ?? ""),
  );
}

function adaptWorkAttempts(raw: ApiStateSnapshot): WorkAttempt[] {
  const entries = new Map<string, WorkAttempt>();
  const checkpointOrder = new Map<string, number>();
  const attemptKey = (task: string, attempt: number) => JSON.stringify([task, attempt]);
  for (const lease of asArray(raw.leases)) {
    const taskId = str(lease, "task_id");
    const number = num(lease, "attempt");
    if (!taskId || number < 1) continue;
    entries.set(attemptKey(taskId, number), {
      taskId, number,
      leaseState: str(lease, "state") || undefined,
      owner: str(lease, "owner") || undefined,
      worker: str(lease, "worker") || undefined,
      endReason: str(lease, "end_reason") || undefined,
    });
  }

  const checkpoints = [
    ...asArray(raw.checkpoints),
    ...asArray(raw.tasks).map((task) => asRecord(task.latest_checkpoint)).filter((row) => row !== undefined),
  ];
  for (const checkpoint of checkpoints) {
    const taskId = str(checkpoint, "task_id");
    const number = num(checkpoint, "attempt");
    const sequence = num(checkpoint, "seq");
    const sha = str(checkpoint, "sha").slice(0, 7);
    if (!taskId || number < 1 || !sha) continue;
    const key = attemptKey(taskId, number);
    if (sequence < (checkpointOrder.get(key) ?? 0)) continue;
    checkpointOrder.set(key, sequence);
    entries.set(key, { ...(entries.get(key) ?? { taskId, number }), checkpoint: sha });
  }
  return [...entries.values()];
}

export function adaptApiState(raw: ApiStateSnapshot): FactorySnapshot {
  const hub = asRecord(raw.hub);
  const hubId = num(hub, "id", 0);
  const nowSec = num(raw, "as_of", Math.floor(Date.now() / 1000));
  const asOf = formatClock(hub?.now ?? nowSec);

  const rawTasks = asArray(raw.tasks);
  const rawLeases = asArray(raw.leases);
  const rawOffers = asArray(raw.offers);

  const activeLeaseByTask = new Map<string, Record<string, unknown>>();
  for (const lease of rawLeases) {
    const st = str(lease, "state");
    if (st === "active" || st === "suspect") {
      activeLeaseByTask.set(str(lease, "task_id"), lease);
    }
  }

  const openOfferByTask = new Map<string, Record<string, unknown>>();
  for (const offer of rawOffers) {
    if (str(offer, "state") === "open") {
      openOfferByTask.set(str(offer, "task_id"), offer);
    }
  }

  const activeScopeOwners = rawTasks
    .filter((t) => {
      const st = str(t, "state").toLowerCase();
      return st === "leased" || st === "running";
    })
    .map((t) => ({ id: str(t, "id"), scope: str(t, "scope") }));

  const laidOutTasks = layoutTasks(
    rawTasks.map((t) => {
      const id = str(t, "id");
      const state = titleCaseTaskState(str(t, "state", "ready"));
      const scope = str(t, "scope");
      const lease = activeLeaseByTask.get(id);
      const offer = openOfferByTask.get(id);
      const cp = asRecord(t.latest_checkpoint);
      const deps = Array.isArray(t.deps)
        ? t.deps.filter((d): d is string => typeof d === "string")
        : [];

      let conflict: string | undefined;
      if (state === "Ready" || state === "Offered") {
        const blocker = activeScopeOwners.find(
          (active) => active.id !== id && scopesRoughlyOverlap(scope, active.scope),
        );
        if (blocker) conflict = blocker.id;
      }

      const owner = str(lease, "owner") || undefined;
      const offeredTo = str(offer, "to_owner") || undefined;
      const cls = str(lease, "class") || str(offer, "class");
      const provider = inferProvider(cls, cls);
      const hb = num(lease, "heartbeat_at", 0);
      const blockerReason =
        !conflict && offeredTo ? `Offered to ${offeredTo}` : undefined;

      return {
        id,
        title: str(t, "title", id),
        state,
        scope,
        done: str(t, "done_when", "Complete task requirements."),
        kind: str(t, "kind") || undefined,
        risk: str(t, "risk", "normal") as "low" | "normal" | "guarded",
        feature: str(t, "feature") || undefined,
        size: str(t, "size") || undefined,
        minTrust: num(t, "min_trust", 2),
        sha: str(t, "result_sha") ? str(t, "result_sha").slice(0, 7) : undefined,
        owner,
        offeredTo,
        blockerReason,
        provider,
        heartbeat: owner && hb > 0 ? formatRelativeAge(hb, nowSec) : undefined,
        deps: deps.length ? deps : undefined,
        conflict,
        attempt: num(t, "attempt", 1),
        maxAttempts: num(t, "max_attempts", 3),
        checkpoint: str(cp, "sha")
          ? str(cp, "sha").slice(0, 7)
          : str(t, "resume_sha")
            ? str(t, "resume_sha").slice(0, 7)
            : undefined,
        priority: capitalize(str(t, "priority", "normal")),
        score: typeof t.score === "number" ? t.score : undefined,
        briefAvailable: Boolean(str(t, "brief_sha256")),
      };
    }),
  );

  const rawRecent = asArray(raw.recent);
  const messages: BusMessage[] = [...rawRecent].reverse().map((m) => ({
    id: num(m, "id", 0),
    at: formatClock(m.ts),
    from: str(m, "from", "hub"),
    role: (str(m, "role", "hub") === "hub"
      ? "Hub"
      : titleCaseRole(str(m, "role"))) as AccessRole | "Hub",
    kind: str(m, "kind", "CHAT"),
    topic: str(m, "topic", "-"),
    body: str(m, "body", ""),
    refs: Array.isArray(m.refs) ? m.refs.filter((r): r is string => typeof r === "string") : [],
    to: Array.isArray(m.to) ? m.to.filter((r): r is string => typeof r === "string") : undefined,
  }));

  const agentMachines = adaptAgentMachines(raw, nowSec);
  const rawMembers = asArray(raw.members);
  const rawAgents = asArray(raw.agents);
  const derived = asRecord(raw.derived);
  const derivedWorkers = asArray(derived?.workers);
  const rawRoles = asArray(raw.roles);
  const rawStanding = asArray(raw.standing);
  const rawKeys = asArray(raw.keys);
  const rawCapacities = asArray(raw.capacities);

  // Merge token members with key-only or standing-only participants.
  const memberByName = new Map<string, Record<string, unknown>>();
  for (const m of rawMembers) {
    const name = str(m, "name");
    if (name) memberByName.set(name, m);
  }
  const allParticipantNames = new Set<string>([
    ...memberByName.keys(),
    ...agentMachines.map(({ owner }) => owner),
    ...rawKeys.map((k) => str(k, "member")).filter(Boolean),
    ...rawStanding.map((s) => str(s, "member")).filter(Boolean),
  ]);

  const contributors: Contributor[] = [...allParticipantNames].map((name) => {
    const m = memberByName.get(name);
    const role = titleCaseRole(str(m, "role", "member"));
    const worker = derivedWorkers.find((w) => str(w, "owner") === name);
    const agent = rawAgents.find((a) => str(a, "owner") === name);
    const cap = rawCapacities.find((c) => str(c, "owner") === name);
    const keyRow = rawKeys.find((k) => str(k, "member") === name);
    const standingRow = rawStanding.find((s) => str(s, "member") === name);
    const heldRole = rawRoles.find((r) => str(r, "holder") === name && str(r, "state") === "held");

    const provider = inferProvider(
        str(worker, "model") || str(agent, "model") || str(m, "hello"),
        str(worker, "class") || str(agent, "class"),
      );

    const rawMode = str(worker, "mode") || str(cap, "mode", "auto");
    const mode: "Auto" | "Own" = rawMode.toLowerCase() === "own" ? "Own" : "Auto";

    const derivedState =
      str(worker, "state") ||
      str(agent, "state") ||
      (bool(m, "online") ? "Idle" : "Offline");
    const seat = heldRole
      ? `${capitalize(str(heldRole, "role"))} · ${str(heldRole, "lease_scope", "full")}`
      : mode === "Own"
        ? "Guest lead"
        : "Worker";

    const tierVal =
      standingRow?.tier !== undefined
        ? String(standingRow.tier)
        : str(m, "trust") === "owner"
          ? "3"
          : str(m, "trust") === "tier0"
            ? "0"
            : "1";

    const countsObj = asRecord(standingRow?.counts);
    const stamps: StampEntry[] = countsObj
      ? Object.entries(countsObj)
          .filter(([, count]) => typeof count === "number" && count > 0)
          .map(([kind, count]) => ({
            kind,
            evidence: `${count} recorded`,
            issuer: "hub",
            at: asOf,
          }))
      : [];

    const missingTier = Array.isArray(standingRow?.missing)
      ? standingRow.missing.filter((item): item is string => typeof item === "string")
      : undefined;

    const openPenalties = asArray(standingRow?.open_penalties).map((p) => ({
      id: num(p, "id", 0),
      kind: str(p, "kind"),
    }));

    const displayHandle = str(keyRow, "handle") || name;

    return {
      name,
      initials: initialsOf(displayHandle),
      role,
      seat,
      tier: tierVal,
      provider,
      machine: str(agent, "machine") || str(cap, "machine") || "local",
      mode,
      state: capitalize(derivedState.replaceAll("_", " ")),
      detail:
        str(worker, "text") ||
        str(agent, "text") ||
        str(worker, "reason") ||
        str(m, "hello") ||
        (bool(m, "online") ? "Online on hub." : "Currently offline."),
      keyId: keyRow ? str(keyRow, "member") : undefined,
      keyState: (str(keyRow, "state") as "active" | "rotated" | "revoked") || undefined,
      stamps: stamps.length > 0 ? stamps : undefined,
      missingTier: missingTier?.length ? missingTier : undefined,
      openPenalties: openPenalties.length ? openPenalties : undefined,
    };
  });

  const subsObj = asRecord(raw.submissions);
  const rawSubs = asArray(subsObj?.newest);
  const submissionCounts = strMap(asRecord(subsObj?.counts));
  const submissions: SubmissionItem[] = rawSubs.map((s) => {
    // Rust `SubRow::to_json` emits `"submission"`, whereas some test/demo inputs use `"sha256"`.
    const sha256 = str(s, "submission") || str(s, "sha256");
    return {
      sha256,
      shortSha: sha256.slice(0, 7),
      task: str(s, "task"),
      attempt: num(s, "attempt", 1),
      member: str(s, "member"),
      base: str(s, "base").slice(0, 7),
      head: str(s, "head").slice(0, 7),
      bytes: num(s, "bytes", 0),
      state: str(s, "state", "received") as SubmissionState,
      risk: str(s, "risk", "normal"),
      tier: num(s, "tier", 0),
      updatedAt: formatClock(num(s, "updated_at", 0)),
    };
  });

  const reviewCounts = strMap(asRecord(raw.review_counts));
  const reviews: ReviewItem[] = asArray(raw.reviews).map((r) => ({
    task: str(r, "task"),
    n: num(r, "n", 1),
    submission: str(r, "submission").slice(0, 7),
    reviewer: str(r, "reviewer"),
    role: bool(r, "audited") || str(r, "role") === "audit" ? "audit" : "required",
    state: str(r, "state", "assigned"),
    verdict: str(r, "verdict") || undefined,
    findings: parseFindings(r.findings),
    pickedBy: str(r, "picked_by") || undefined,
  }));

  const landing: LandingItem[] = asArray(raw.landing).map((l) => ({
    // Rust `LandingRow` serializes `ref_name` with `#[serde(rename = "ref")]`.
    refName: str(l, "ref") || str(l, "ref_name"),
    sha: str(l, "sha").slice(0, 7),
    taskId: str(l, "task_id"),
    state: str(l, "state", "queued"),
    priority: str(l, "priority", "normal"),
    batchId: str(l, "batch_id") || undefined,
    tries: typeof l.tries === "number" ? l.tries : undefined,
    failStep: str(l, "fail_step") || undefined,
    failClass: str(l, "fail_class") || undefined,
    approvedBy: str(l, "approved_by") || undefined,
    enqueuedAt: formatClock(num(l, "enqueued_at", 0)),
  }));

  const batches: BatchItem[] = asArray(raw.batches).map((b) => {
    const members = Array.isArray(b.members) ? b.members : [];
    const baseSha = str(b, "base_sha") || str(b, "head_sha");
    const result = str(b, "result") || str(b, "state") || "running";
    return {
      batchId: str(b, "batch_id"),
      state: result,
      count: members.length || num(b, "count", 0),
      headSha: baseSha ? baseSha.slice(0, 7) : undefined,
      note:
        str(b, "note") ||
        (num(b, "verifications", 0) > 0 ? `${num(b, "verifications")} verifications` : undefined),
    };
  });

  const features: FeatureItem[] = asArray(raw.features).map((f) => {
    const stateObj = asRecord(f.state) ?? {};
    const states: Record<string, string> = {};
    for (const [k, v] of Object.entries(stateObj)) {
      if (typeof v === "string") states[k] = v;
    }
    return {
      id: str(f, "id"),
      area: str(f, "area"),
      areaTitle: str(f, "area_title") || capitalize(str(f, "area")),
      title: str(f, "title"),
      paths: Array.isArray(f.paths) ? f.paths.filter((p): p is string => typeof p === "string") : [],
      depends: Array.isArray(f.depends) ? f.depends.filter((d): d is string => typeof d === "string") : [],
      how: str(f, "how") || undefined,
      proofs: asArray(f.proofs).map((proof) => ({
        profile: str(proof, "profile"),
        scenario: str(proof, "scenario"),
        milestone: str(proof, "milestone"),
      })).filter(({ profile, scenario, milestone }) => profile && scenario && milestone),
      openOrders: num(f, "open_orders", 0),
      openTasks: num(f, "open_tasks", 0),
      openRuns: num(f, "open_runs", 0),
      states,
    };
  });

  const matrixBlocksObj = asRecord(raw.matrix_blocks);
  const matrixBlocksNote = matrixBlocksObj
    ? {
        kept: bool(matrixBlocksObj, "kept", true),
        text: str(matrixBlocksObj, "text"),
      }
    : undefined;

  const matrix: MatrixCell[] = asArray(raw.matrix).map((c) => {
    // Rust `report::matrix` emits `state` (`works | fails | conflict | unsettled`).
    const rawState = str(c, "state") || str(c, "outcome", "unsettled");
    const outcome: MatrixCell["outcome"] =
      rawState === "works"
        ? "works"
        : rawState === "fails" || rawState === "broken"
          ? "broken"
          : rawState === "conflict" || rawState === "flaky"
            ? "flaky"
            : "pending";
    return {
      profile: str(c, "profile"),
      scenario: str(c, "scenario") || undefined,
      machineKey: str(c, "machine_key"),
      outcome,
      reports: num(c, "reports", 0),
      quorum: num(c, "quorum", 3),
      build: str(c, "build").slice(0, 7),
      channel: str(c, "channel") || undefined,
    };
  });

  const runOrders: RunOrderItem[] = asArray(raw.run_orders).map((o) => {
    const needs = asRecord(o.needs);
    return {
      id: str(o, "id"),
      build: str(o, "build").slice(0, 7),
      channel: str(o, "channel"),
      profile: str(o, "profile"),
      scenario: str(o, "scenario"),
      needsOs: str(needs, "os") || undefined,
      needsGpu: str(needs, "gpu") || undefined,
      reports: num(o, "reports", 0),
      wanted: num(o, "wanted", 3),
    };
  });

  const regressObj = asRecord(raw.regress);
  const regressions: RegressionCheck[] = asArray(regressObj?.checks).map((c) => {
    const rawStatus = str(c, "status", "baseline_good");
    const status: RegressionCheck["status"] =
      rawStatus === "regression_confirmed" || rawStatus === "regression"
        ? "regression"
        : rawStatus === "need_more_runs" || rawStatus === "confirm_runs_needed"
          ? "need_more_runs"
          : bool(c, "ignored_as_noise") || rawStatus === "noise_ignored"
            ? "noise_ignored"
            : "good";
    const bisectTask = asRecord(c.bisect_task);
    const firstTask = asArray(c.tasks)[0];
    return {
      sha: str(c, "sha").slice(0, 7),
      machineClass: str(c, "machine_class"),
      scenario: str(c, "scenario"),
      metric: str(c, "metric"),
      status,
      candidateMedian: num(c, "new", num(c, "candidate_median", 0)),
      baselineMedian:
        typeof c.base === "number"
          ? c.base
          : typeof c.baseline_median === "number"
            ? c.baseline_median
            : undefined,
      madGuard:
        typeof c.mad === "number"
          ? c.mad
          : typeof c.mad_guard === "number"
            ? c.mad_guard
            : undefined,
      linkedTask: str(bisectTask, "id") || str(firstTask, "id") || undefined,
    };
  });

  const pools: QuotaPool[] = asArray(raw.pools).map((p) => {
    const poolName = str(p, "pool");
    const vendor = str(p, "vendor");
    const redacted = p.owner === undefined && p.used_pct === undefined;
    return {
      owner: str(p, "owner", "public"),
      vendor,
      provider: inferProvider(vendor, poolName) ?? "Claude",
      pool: poolName,
      window: str(p, "window", "5h"),
      state: str(p, "state", "open") as QuotaPool["state"],
      drain: bool(p, "drain", false),
      usedPct: num(p, "used_pct", 0),
      committedPct: num(p, "committed_pct", 0),
      linePct: num(p, "line_pct", 80),
      stopAtPct: num(p, "stop_at_pct", 90),
      reservePct: num(p, "reserve_pct", 10),
      resetsAt: num(p, "resets_at", 0),
      burnPctPerHour: num(p, "burn_pct_per_hour", 0),
      forecastLabel: str(p, "forecast_label", "steady"),
      source: str(p, "source", "inferred"),
      confidence: num(p, "confidence", 0),
      band: str(p, "band", "medium") as QuotaPool["band"],
      ageSecs: num(p, "age_secs", 0),
      stale: bool(p, "stale", false),
      activeLeases: num(p, "active_leases", 0),
      waitingTasks: asArray(p.waiting_tasks).map((w) => ({
        id: str(w, "id"),
        title: str(w, "title"),
        kind: str(w, "kind"),
        priority: str(w, "priority"),
        reason: str(w, "reason"),
      })),
      redacted,
    };
  });

  const roleLeases: RoleLease[] = rawRoles.map((r) => ({
    role: str(r, "role"),
    scope: str(r, "scope", "global"),
    holder: typeof r.holder === "string" ? r.holder : null,
    leaseScope: str(r, "lease_scope", "full"),
    epoch: num(r, "epoch", 0),
    state: str(r, "state", "vacant") as RoleLease["state"],
    frozen: bool(r, "frozen", false),
    lastReason: str(r, "last_reason", ""),
    handoverNote: typeof r.handover_note === "string" ? r.handover_note : undefined,
  }));

  const rawAlerts = asArray(raw.alerts).length ? asArray(raw.alerts) : asArray(derived?.alerts);
  const alerts: FactoryAlert[] = rawAlerts.map((a) => {
    const alertState = str(a, "state");
    return {
      key: str(a, "key"),
      code: str(a, "code"),
      message: str(a, "text") || str(a, "message") || str(a, "code"),
      at: formatClock(num(a, "last_at", 0) || num(a, "opened_at", 0) || a.at),
      acked: alertState ? alertState !== "open" : bool(a, "acked", false),
    };
  });

  const claims: ClaimItem[] = asArray(raw.claims).map((c) => ({
    topic: str(c, "topic"),
    holder: str(c, "holder"),
    since: formatClock(c.since),
  }));

  const schedObj = asRecord(raw.scheduler);
  const safeModeObj = asRecord(schedObj?.safe_mode);
  const scheduler: SchedulerStatus = {
    mode: str(schedObj, "mode", "enforce") as SchedulerStatus["mode"],
    safeMode: bool(safeModeObj, "active", false),
    decisionCount: num(schedObj, "decision_count", 0),
    activePauses: Array.isArray(schedObj?.active_pauses)
      ? schedObj.active_pauses.filter((p): p is string => typeof p === "string")
      : [],
  };

  return {
    hubId,
    asOf,
    tasks: laidOutTasks,
    messages,
    contributors,
    agentMachines,
    workAttempts: adaptWorkAttempts(raw),
    submissions,
    submissionCounts,
    reviews,
    reviewCounts,
    landing,
    batches,
    features,
    matrix,
    matrixBlocksNote,
    runOrders,
    regressions,
    pools,
    roleLeases,
    alerts,
    claims,
    scheduler,
    briefs: {},
  };
}

/** Applies the same public-read redaction contract as `bus/src/door/mod.rs::redact_state`. */
export function redactSnapshotForPublic(snapshot: FactorySnapshot): FactorySnapshot {
  return {
    ...snapshot,
    agentMachines: snapshot.agentMachines.filter(({ agents }) => agents.length > 0)
      .map(({ slots: _slots, ...machine }) => machine),
    pools: snapshot.pools.map((p) => ({
      ...p,
      owner: "public",
      usedPct: 0,
      committedPct: 0,
      burnPctPerHour: 0,
      activeLeases: 0,
      waitingTasks: [],
      redacted: true,
    })),
    alerts: snapshot.alerts.filter((a) => a.code !== "COORD_BUDGET"),
    messages: snapshot.messages.filter((m) => {
      if (m.kind === "FACT") {
        return ![
          "quota/",
          "spend/",
          "estimate/",
          "capacity/",
          "pool/",
          "owner/",
          "class/",
          "pause/",
          "decision/",
          "metric/",
        ].some((prefix) => m.topic.startsWith(prefix));
      }
      return true;
    }),
  };
}
