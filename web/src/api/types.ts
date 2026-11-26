export const TASK_STATES = [
  "Draft",
  "Ready",
  "Offered",
  "Leased",
  "Running",
  "Delivered",
  "Landing",
  "Closed",
  "Stuck",
  "Cancelled",
] as const;

export type TaskState = (typeof TASK_STATES)[number];
export type AccessRole = "Orchestrator" | "Member" | "Observer" | "Public";
export type AgentProvider = "Claude" | "Codex" | "Gemini";
export type DataSourceMode = "demo" | "live";

export type HubIdentity = {
  name: string;
  role: AccessRole;
  tier?: number;
};

export type WorkTask = {
  id: string;
  title: string;
  state: TaskState;
  x: number;
  y: number;
  scope: string;
  done: string;
  kind?: string;
  risk?: "low" | "normal" | "guarded";
  feature?: string;
  size?: string;
  minTrust?: number;
  sha?: string;
  people?: string[];
  owner?: string;
  offeredTo?: string;
  provider?: AgentProvider;
  heartbeat?: string;
  deps?: string[];
  conflict?: string;
  blockerReason?: string;
  attempt?: number;
  maxAttempts?: number;
  checkpoint?: string;
  priority?: string;
  score?: number;
  briefAvailable?: boolean;
};

export type BusMessage = {
  id: number;
  at: string;
  from: string;
  role: AccessRole | "Hub";
  kind: string;
  topic: string;
  body: string;
  refs: string[];
  to?: string[];
};

export type StampEntry = {
  kind: string;
  evidence: string;
  issuer: string;
  at: string;
};

export type Contributor = {
  name: string;
  initials: string;
  role: "Orchestrator" | "Member" | "Observer";
  seat: string;
  tier: string;
  provider: AgentProvider;
  machine: string;
  mode: "Auto" | "Own";
  state: string;
  detail: string;
  keyId?: string;
  keyState?: "active" | "rotated" | "revoked";
  linkedAccount?: string;
  stamps?: StampEntry[];
  missingTier?: string[];
  openPenalties?: { id: number; kind: string }[];
};

export type SubmissionState =
  | "received"
  | "gate_failed"
  | "gated"
  | "verifying"
  | "verify_failed"
  | "verified"
  | "reviewing"
  | "review_failed"
  | "approved"
  | "landed"
  | "rejected";

export type SubmissionItem = {
  sha256: string;
  shortSha: string;
  task: string;
  attempt: number;
  member: string;
  base: string;
  head: string;
  bytes: number;
  state: SubmissionState;
  risk: string;
  tier: number;
  updatedAt: string;
  gatePassed?: boolean;
  gateFindings?: number;
  verifyRunId?: number;
  verifyState?: string;
  verifyConclusion?: string;
  failingStep?: string;
  reviewSummary?: string;
  auditAssignedTo?: string;
};

export type ReviewItem = {
  task: string;
  n: number;
  submission: string;
  reviewer: string;
  role: "required" | "audit";
  state: string;
  verdict?: string;
  findings?: string;
  pickedBy?: string;
  due?: string;
  crossFamily?: boolean;
};

export type LandingItem = {
  refName: string;
  sha: string;
  taskId: string;
  state: string;
  priority: string;
  batchId?: string;
  tries?: number;
  failStep?: string;
  failClass?: string;
  approvedBy?: string;
  enqueuedAt: string;
};

export type BatchItem = {
  batchId: string;
  state: string;
  count: number;
  headSha?: string;
  note?: string;
};

export type FeatureItem = {
  id: string;
  area: string;
  areaTitle: string;
  title: string;
  paths: string[];
  depends: string[];
  how?: string;
  openOrders: number;
  openTasks: number;
  openRuns: number;
  states: Record<string, string>;
};

export type MatrixCell = {
  profile: string;
  scenario: string;
  machineKey: string;
  outcome: "works" | "broken" | "flaky" | "pending";
  reports: number;
  quorum: number;
  build: string;
  channel: string;
};

export type RunOrderItem = {
  id: string;
  build: string;
  channel: string;
  profile: string;
  scenario: string;
  needsOs?: string;
  needsGpu?: string;
  reports: number;
  wanted: number;
};

export type RegressionCheck = {
  sha: string;
  machineClass: string;
  scenario: string;
  metric: string;
  status: "good" | "need_more_runs" | "noise_ignored" | "regression";
  candidateMedian: number;
  baselineMedian?: number;
  madGuard?: number;
  linkedTask?: string;
};

export type WaitingPoolTask = {
  id: string;
  title: string;
  kind: string;
  priority: string;
  reason: string;
};

export type QuotaPool = {
  owner: string;
  vendor: string;
  provider: AgentProvider;
  pool: string;
  window: string;
  state: "open" | "drain" | "throttled" | "closed" | "paused";
  drain: boolean;
  usedPct: number;
  committedPct: number;
  linePct: number;
  stopAtPct: number;
  reservePct: number;
  resetsAt: number;
  burnPctPerHour: number;
  forecastLabel: string;
  source: string;
  confidence: number;
  band: "high" | "medium" | "low";
  ageSecs: number;
  stale: boolean;
  activeLeases: number;
  waitingTasks: WaitingPoolTask[];
  redacted?: boolean;
};

export type RoleLease = {
  role: string;
  scope: string;
  holder: string | null;
  leaseScope: string;
  epoch: number;
  state: "held" | "handing_over" | "vacant";
  frozen: boolean;
  lastReason: string;
  handoverNote?: string;
};

export type FactoryAlert = {
  key: string;
  code: string;
  message: string;
  at: string;
  acked?: boolean;
};

export type SchedulerStatus = {
  mode: "off" | "shadow" | "enforce";
  safeMode: boolean;
  decisionCount: number;
  activePauses: string[];
};

export type ClaimItem = {
  topic: string;
  holder: string;
  since: string;
};

export type FactorySnapshot = {
  hubId: number;
  asOf: string;
  tasks: WorkTask[];
  messages: BusMessage[];
  contributors: Contributor[];
  submissions: SubmissionItem[];
  submissionCounts?: Record<string, number>;
  reviews: ReviewItem[];
  reviewCounts?: Record<string, number>;
  landing: LandingItem[];
  batches: BatchItem[];
  features: FeatureItem[];
  matrix: MatrixCell[];
  matrixBlocksNote?: { kept: boolean; text: string };
  runOrders: RunOrderItem[];
  regressions: RegressionCheck[];
  pools: QuotaPool[];
  roleLeases: RoleLease[];
  alerts: FactoryAlert[];
  claims?: ClaimItem[];
  scheduler: SchedulerStatus;
  briefs: Record<string, string>;
};

export type ApiStateSnapshot = Record<string, unknown>;
