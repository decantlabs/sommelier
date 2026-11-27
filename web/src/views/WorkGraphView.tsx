import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent, ReactNode, WheelEvent } from "react";
import {
  ArrowRight,
  ArrowSquareOut,
  ArrowsOut,
  CaretDown,
  CaretUp,
  Crosshair,
  FileText,
  FlowArrow,
  Minus,
  Pause,
  Play,
  Plus,
  SquaresFour,
} from "@phosphor-icons/react";
import { layoutTasks } from "../api/adapter";
import { buildScopeOptions, parseMessageBody, scopePrefixOf } from "../api/format";
import type {
  AccessRole,
  BusMessage,
  FactoryAlert,
  SchedulerStatus,
  SubmissionItem,
  WorkTask,
} from "../api/types";
import {
  Button,
  CodeChip,
  DefinitionList,
  InspectorPanel,
  ProviderLabel,
  SegmentedControl,
  StatusLabel,
  TaskNode,
  messageKindTone,
  taskStateTone,
} from "../components";
import type { DefinitionItem } from "../components";
import { readCanvasTokens } from "../design/tokens";
import "./work-graph.css";

type VisibilityMode = "all" | "active" | "attention";
type ViewMode = "graph" | "grid";

type GraphProps = {
  tasks: WorkTask[];
  messages: BusMessage[];
  selected: WorkTask | undefined;
  onSelect: (id: string | null) => void;
  onMessages: (topic: string) => void;
  onInspectBrief: (id: string) => void;
  onOpenSubmission: (taskId: string) => void;
  onOpenLoop: (taskId: string) => void;
  submissions: SubmissionItem[];
  alerts: FactoryAlert[];
  scheduler: SchedulerStatus;
  role: AccessRole;
  onAckAlert: (key: string) => void;
  onTogglePause: () => void;
  onOpenTask: (id: string) => void;
  onOpenContributors: () => void;
  onOpenPipeline: () => void;
  search: string;
  scope: string;
  onScopeChange: (scope: string) => void;
  needsAttention: boolean;
  onNeedsAttentionChange: (value: boolean) => void;
  presenceProviders: ReactNode[];
  runningAgentCount: number;
};

const cardWidth = 316;
const cardHeight = 160;

function Connections({
  tasks,
  width,
  height,
}: {
  tasks: WorkTask[];
  width: number;
  height: number;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const el = canvas.current;
    const ctx = el?.getContext("2d");
    if (!el || !ctx) return;

    const palette = readCanvasTokens(el);
    const byId = new Map(tasks.map((task) => [task.id, task]));
    const edges: { from: WorkTask; to: WorkTask; conflict: boolean; active: boolean }[] = [];
    for (const task of tasks) {
      const taskActive = task.state === "Running" || task.state === "Leased" || task.state === "Landing";
      for (const dep of task.deps ?? []) {
        const previous = byId.get(dep);
        if (previous) edges.push({ from: previous, to: task, conflict: false, active: taskActive });
      }
      const blocker = task.conflict && byId.get(task.conflict);
      if (blocker) edges.push({ from: blocker, to: task, conflict: true, active: false });
    }

    const reducedMotion =
      typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    const hasActiveEdge = edges.some((e) => e.active);
    let rafId = 0;

    function bezierPoint(p0: number, p1: number, p2: number, p3: number, t: number): number {
      const u = 1 - t;
      return u * u * u * p0 + 3 * u * u * t * p1 + 3 * u * t * t * p2 + t * t * t * p3;
    }

    function draw(nowMs: number) {
      const paint = ctx!;
      paint.setTransform(2, 0, 0, 2, 0, 0);
      paint.clearRect(0, 0, width, height);
      paint.lineWidth = 1.7;

      const phase = (nowMs % 2800) / 2800;

      for (let i = 0; i < edges.length; i += 1) {
        const { from, to, conflict, active } = edges[i];
        const sameColumn = Math.abs(from.x - to.x) < 20;
        const x1 = sameColumn ? from.x + cardWidth / 2 : from.x + cardWidth;
        const y1 = sameColumn ? from.y + cardHeight : from.y + cardHeight / 2;
        const x2 = sameColumn ? to.x + cardWidth / 2 : to.x;
        const y2 = sameColumn ? to.y : to.y + cardHeight / 2;
        const midX = (x1 + x2) / 2;
        const color = conflict ? palette.conflict : palette.edge;

        paint.strokeStyle = color;
        paint.fillStyle = color;
        paint.globalAlpha = conflict ? 0.85 : active ? 0.88 : 0.55;
        paint.setLineDash(conflict ? [7, 5] : []);
        paint.beginPath();
        paint.moveTo(x1, y1);
        paint.bezierCurveTo(midX, y1, midX, y2, x2, y2);
        paint.stroke();
        paint.setLineDash([]);

        for (const [x, y] of [
          [x1, y1],
          [x2, y2],
        ]) {
          paint.beginPath();
          paint.arc(x, y, 4, 0, 2 * Math.PI);
          paint.fillStyle = palette.portFill;
          paint.fill();
          paint.stroke();
        }

        if (active && !reducedMotion) {
          const t = (phase + i * 0.23) % 1;
          const px = bezierPoint(x1, midX, midX, x2, t);
          const py = bezierPoint(y1, y1, y2, y2, t);
          paint.globalAlpha = Math.sin(t * Math.PI) * 0.95;
          paint.fillStyle = "#79d8a5";
          paint.beginPath();
          paint.arc(px, py, 3, 0, Math.PI * 2);
          paint.fill();
        }
      }
      paint.globalAlpha = 1;
    }

    draw(performance.now());

    if (hasActiveEdge && !reducedMotion) {
      const tick = (ts: number) => {
        draw(ts);
        rafId = window.requestAnimationFrame(tick);
      };
      rafId = window.requestAnimationFrame(tick);
    }

    return () => {
      if (rafId) window.cancelAnimationFrame(rafId);
    };
  }, [tasks, width, height]);

  return (
    <canvas
      ref={canvas}
      className="connections"
      style={{ width, height }}
      width={width * 2}
      height={height * 2}
      aria-hidden="true"
    />
  );
}

function TaskInspector({
  task,
  hasSubmission,
  onSelect,
  onMessages,
  onInspectBrief,
  onOpenSubmission,
  onOpenLoop,
}: {
  task: WorkTask;
  hasSubmission: boolean;
  onSelect: GraphProps["onSelect"];
  onMessages: GraphProps["onMessages"];
  onInspectBrief: GraphProps["onInspectBrief"];
  onOpenSubmission: GraphProps["onOpenSubmission"];
  onOpenLoop: GraphProps["onOpenLoop"];
}) {
  const definitions: DefinitionItem[] = [
    {
      label: "Task state",
      value: (
        <StatusLabel variant="pill" tone={taskStateTone(task.state)}>
          {task.state}
        </StatusLabel>
      ),
    },
    ...(task.blockerReason && !task.conflict ? [{ label: "Dispatch", value: task.blockerReason }] : []),
    { label: "Priority", value: task.priority ?? "Normal" },
    { label: "Attempt", value: `${task.attempt ?? 1} of ${task.maxAttempts ?? 3}` },
    { label: "Min. trust", value: `Tier ${task.minTrust ?? 2}` },
    ...(task.risk ? [{ label: "Risk", value: task.risk }] : []),
    ...(task.kind ? [{ label: "Order kind", value: <span className="mono">{task.kind}</span> }] : []),
    ...(task.feature ? [{ label: "Feature", value: <span className="mono">{task.feature}</span> }] : []),
    ...(task.owner
      ? [
          {
            label: "Lease holder",
            value: task.provider ? (
              <ProviderLabel provider={task.provider} suffix={` · ${task.owner}`} />
            ) : (
              task.owner
            ),
          },
        ]
      : []),
    ...(task.heartbeat ? [{ label: "Last status", value: task.heartbeat }] : []),
    ...(task.deps?.length ? [{ label: "Dependencies", value: task.deps.join(", ") }] : []),
  ];

  return (
    <InspectorPanel
      mode="docked"
      className="graph-inspector-docked"
      ariaLabel={`Details for ${task.id}`}
      eyebrow={<span className="task-id">{task.id}</span>}
      title={task.title}
      onClose={() => onSelect(null)}
      closeLabel="Close task details"
    >
      {task.conflict && (
        <div className="graph-inspector-callout">
          <strong>Waiting on active scope</strong>
          <p>{task.conflict} holds an active lease in the same scope. Dispatch resumes once it releases.</p>
          <Button variant="link" onClick={() => onSelect(task.conflict!)}>
            Inspect blocking task {task.conflict} <ArrowRight size={14} />
          </Button>
        </div>
      )}

      <p className="graph-inspector-done">{task.done}</p>

      {task.scope && (
        <div className="graph-inspector-scope">
          <CodeChip small>{task.scope}</CodeChip>
        </div>
      )}

      <DefinitionList items={definitions} />

      <div className="ui-inspector-actions ui-inspector-actions-docked">
        <Button variant="link" onClick={() => onMessages(`task/${task.id}`)}>
          Task messages <ArrowRight size={14} />
        </Button>
        {task.checkpoint && (
          <small className="inspector-actions-note">
            Checkpoint <code>{task.checkpoint}</code>
          </small>
        )}
      </div>

      <div className="inspector-sublinks">
        <Button variant="secondary" size="sm" onClick={() => onOpenLoop(task.id)}>
          View work loop <ArrowRight size={14} />
        </Button>
        {task.briefAvailable && (
          <Button variant="secondary" size="sm" onClick={() => onInspectBrief(task.id)}>
            <FileText size={14} /> Inspect brief
          </Button>
        )}
        {hasSubmission && (
          <Button variant="secondary" size="sm" onClick={() => onOpenSubmission(task.id)}>
            View submission <ArrowRight size={14} />
          </Button>
        )}
      </div>
    </InspectorPanel>
  );
}

export default function WorkGraphView({
  tasks,
  messages,
  selected,
  onSelect,
  onMessages,
  onInspectBrief,
  onOpenSubmission,
  onOpenLoop,
  submissions,
  alerts,
  scheduler,
  role,
  onAckAlert,
  onTogglePause,
  onOpenTask,
  onOpenContributors,
  onOpenPipeline,
  search,
  scope,
  onScopeChange,
  needsAttention,
  onNeedsAttentionChange,
  presenceProviders,
  runningAgentCount,
}: GraphProps) {
  const viewport = useRef<HTMLDivElement>(null);
  const drag = useRef<{ x: number; y: number; left: number; top: number } | null>(null);
  const [isPanning, setPanning] = useState(false);
  const [viewSize, setViewSize] = useState({ width: 1200, height: 680 });
  const [zoom, setZoom] = useState(1);
  const [activityOpen, setActivityOpen] = useState(false);
  const [visibility, setVisibility] = useState<VisibilityMode>(needsAttention ? "attention" : "all");
  const [viewMode, setViewMode] = useState<ViewMode>("graph");
  const searchText = search.trim().toLowerCase();

  useEffect(() => {
    setVisibility(needsAttention ? "attention" : (prev) => (prev === "attention" ? "all" : prev));
  }, [needsAttention]);

  useEffect(() => {
    if (!viewport.current) return;
    const observer = new ResizeObserver(([entry]) => {
      setViewSize({
        width: Math.max(320, entry.contentRect.width),
        height: Math.max(320, entry.contentRect.height),
      });
    });
    observer.observe(viewport.current);
    return () => observer.disconnect();
  }, [viewMode]);

  const scopeOptions = useMemo(() => buildScopeOptions(tasks), [tasks]);

  const activeCount = useMemo(
    () => tasks.filter((t) => t.state !== "Cancelled" && t.state !== "Closed").length,
    [tasks],
  );
  const attentionCount = useMemo(
    () =>
      tasks.filter(
        (task) =>
          Boolean(task.conflict) ||
          task.state === "Stuck" ||
          tasks.some((other) => other.conflict === task.id),
      ).length,
    [tasks],
  );

  const isFiltered = Boolean(searchText || scope || visibility !== "all");

  const filteredTasks = useMemo(() => {
    const matched = tasks.filter((task) => {
      const matchesSearch =
        !searchText ||
        [task.id, task.title, task.scope, task.feature ?? "", task.done].some((value) =>
          value.toLowerCase().includes(searchText),
        );
      const matchesScope = !scope || scopePrefixOf(task.scope) === scope || task.scope.startsWith(scope);
      const isAttention =
        Boolean(task.conflict) ||
        task.state === "Stuck" ||
        tasks.some((other) => other.conflict === task.id);
      const isActive = task.state !== "Cancelled" && task.state !== "Closed";
      const matchesVis =
        visibility === "all" ||
        (visibility === "active" && isActive) ||
        (visibility === "attention" && isAttention);
      return matchesSearch && matchesScope && matchesVis;
    });

    // Preserve hand-crafted demo positions when viewing the full reference demo set;
    // dynamically re-pack filtered/live tasks so results always appear cleanly in view.
    const isDemoReference =
      !isFiltered &&
      matched.some((t) => t.id === "SOM-130" && t.x === 105) &&
      matched.some((t) => t.id === "SOM-136" && t.x === 572);

    return isDemoReference ? matched : layoutTasks(matched);
  }, [isFiltered, scope, searchText, tasks, visibility]);

  const hasEdges = useMemo(() => {
    const ids = new Set(filteredTasks.map((t) => t.id));
    return filteredTasks.some(
      (t) => (t.conflict && ids.has(t.conflict)) || (t.deps ?? []).some((d) => ids.has(d)),
    );
  }, [filteredTasks]);

  const findTask = (id: string) => filteredTasks.find((task) => task.id === id);
  const active = selected && findTask(selected.id) ? findTask(selected.id) : undefined;

  const contentBounds = useMemo(() => {
    if (filteredTasks.length === 0) {
      return { width: viewSize.width, height: viewSize.height };
    }
    const maxX = Math.max(...filteredTasks.map((t) => t.x + cardWidth)) + 36;
    const maxY = Math.max(...filteredTasks.map((t) => t.y + cardHeight)) + 96;
    return {
      width: Math.max(viewSize.width, maxX),
      height: Math.max(viewSize.height, maxY),
    };
  }, [filteredTasks, viewSize.height, viewSize.width]);

  const fitScale = useMemo(() => {
    if (filteredTasks.length === 0) return 1;
    const rawW = Math.max(...filteredTasks.map((t) => t.x + cardWidth)) + 88;
    const rawH = Math.max(...filteredTasks.map((t) => t.y + cardHeight)) + 104;
    const scaleX = viewSize.width / rawW;
    const scaleY = viewSize.height / rawH;
    return Math.max(0.45, Math.min(1, Math.min(scaleX, scaleY)));
  }, [filteredTasks, viewSize.height, viewSize.width]);

  const scale = zoom;

  const fitGraph = useCallback(() => {
    setZoom(Number(fitScale.toFixed(2)));
    viewport.current?.scrollTo({ left: 0, top: 0, behavior: "smooth" });
  }, [fitScale]);

  // Reset scroll to top-left whenever filters change so matched nodes are immediately visible.
  useEffect(() => {
    viewport.current?.scrollTo({ left: 0, top: 0 });
  }, [searchText, scope, visibility]);

  const showReferenceLabels =
    !isFiltered &&
    findTask("SOM-130")?.x === 105 &&
    findTask("SOM-136")?.x === 572 &&
    findTask("SOM-142")?.x === 570;

  function startPan(event: PointerEvent<HTMLDivElement>) {
    const target = event.target as HTMLElement;
    const area = event.currentTarget;
    if (event.pointerType !== "mouse" || event.button !== 0 || target.closest("button, aside, a")) {
      return;
    }
    if (area.scrollWidth <= area.clientWidth && area.scrollHeight <= area.clientHeight) return;
    drag.current = { x: event.clientX, y: event.clientY, left: area.scrollLeft, top: area.scrollTop };
    area.setPointerCapture(event.pointerId);
    setPanning(true);
  }

  function movePan(event: PointerEvent<HTMLDivElement>) {
    if (!drag.current) return;
    event.currentTarget.scrollTo(
      drag.current.left + drag.current.x - event.clientX,
      drag.current.top + drag.current.y - event.clientY,
    );
  }

  function stopPan() {
    drag.current = null;
    setPanning(false);
  }

  function handleWheel(event: WheelEvent<HTMLDivElement>) {
    if (!event.ctrlKey && !event.metaKey) return;
    event.preventDefault();
    const delta = event.deltaY < 0 ? 0.1 : -0.1;
    setZoom((prev) => Number(Math.max(0.4, Math.min(1.5, prev + delta)).toFixed(2)));
  }

  const openAlerts = alerts.filter((a) => !a.acked);
  const isPaused = scheduler.activePauses.length > 0;
  const recentActivity = messages.slice(0, 5);
  const latestMessage = messages[0];
  const latestParsed = latestMessage ? parseMessageBody(latestMessage) : null;
  const latestRef = latestMessage?.refs.find((r) => !r.startsWith("state:")) ?? latestMessage?.topic;

  return (
    <>
      <div className="graph-shell">
        <div className="graph-toolbar">
          <div className="graph-toolbar-left">
            <SegmentedControl
              ariaLabel="Task visibility"
              size="sm"
              value={visibility}
              onChange={(next) => {
                setVisibility(next);
                onNeedsAttentionChange(next === "attention");
              }}
              options={[
                { value: "all", label: `All (${tasks.length})` },
                { value: "active", label: `Active (${activeCount})` },
                { value: "attention", label: `Needs attention (${attentionCount})` },
              ]}
            />
            {scopeOptions.length > 0 && (
              <label className="scope-select">
                <span>Scope</span>
                <select
                  aria-label="Scope"
                  value={scope}
                  onChange={(event) => onScopeChange(event.target.value)}
                >
                  <option value="">All scopes</option>
                  {scopeOptions.map((opt) => (
                    <option key={opt.value} value={opt.value}>
                      {opt.label}
                    </option>
                  ))}
                </select>
              </label>
            )}
            {(searchText || scope || visibility !== "all") && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setVisibility("all");
                  onNeedsAttentionChange(false);
                  onScopeChange("");
                }}
              >
                Reset
              </Button>
            )}
          </div>

          <div className="graph-toolbar-right">
            {viewMode === "graph" && hasEdges && (
              <div className="legend">
                <span>
                  <i /> Dependency
                </span>
                <span>
                  <i /> Scope conflict
                </span>
              </div>
            )}
            <div className="view-mode-switch" role="group" aria-label="Layout mode">
              <button
                type="button"
                aria-pressed={viewMode === "graph"}
                onClick={() => setViewMode("graph")}
                title="Spatial work graph"
              >
                <FlowArrow size={15} /> Graph
              </button>
              <button
                type="button"
                aria-pressed={viewMode === "grid"}
                onClick={() => setViewMode("grid")}
                title="Compact card grid"
              >
                <SquaresFour size={15} /> Grid
              </button>
            </div>
          </div>
        </div>

        <div className={`graph-workspace ${active ? "has-inspector" : ""}`}>
          <div className="graph-body">
            {viewMode === "grid" ? (
              <div className="graph-grid-scroll">
                {filteredTasks.length ? (
                  <div className="graph-grid">
                    {filteredTasks.map((task) => (
                      <TaskNode
                        key={task.id}
                        task={task}
                        positioned={false}
                        selected={active?.id === task.id}
                        onSelect={() => onSelect(active?.id === task.id ? null : task.id)}
                      />
                    ))}
                  </div>
                ) : (
                  <div className="empty-graph-card">
                    <strong>
                      {tasks.length === 0 ? "No tasks on this hub yet" : "No matching tasks"}
                    </strong>
                    <p>
                      {tasks.length === 0
                        ? "Create a task or wait for the orchestrator to publish work to the bus."
                        : "Try clearing the search query or switching back to All tasks."}
                    </p>
                  </div>
                )}
              </div>
            ) : (
              <div
                className={`graph-viewport ${isPanning ? "panning" : ""}`}
                ref={viewport}
                onPointerDown={startPan}
                onPointerMove={movePan}
                onPointerUp={stopPan}
                onPointerCancel={stopPan}
                onLostPointerCapture={stopPan}
                onWheel={handleWheel}
              >
                {filteredTasks.length ? (
                  <div
                    className="graph-size"
                    style={{
                      width: contentBounds.width * scale,
                      height: contentBounds.height * scale,
                    }}
                  >
                    <section
                      aria-label="Task graph canvas"
                      className="world"
                      style={{
                        width: contentBounds.width,
                        height: contentBounds.height,
                        transform: `scale(${scale})`,
                      }}
                    >
                      <Connections
                        tasks={filteredTasks}
                        width={contentBounds.width}
                        height={contentBounds.height}
                      />
                      {showReferenceLabels && (
                        <>
                          <span className="edge-label dependency-met">dependency satisfied</span>
                          <span className="edge-label scope-conflict">overlapping scope</span>
                          {findTask("SOM-145") && (
                            <span className="edge-label dependency-required">required before dispatch</span>
                          )}
                        </>
                      )}
                      {filteredTasks.map((task) => (
                        <TaskNode
                          key={task.id}
                          task={task}
                          selected={active?.id === task.id}
                          onSelect={() => onSelect(active?.id === task.id ? null : task.id)}
                        />
                      ))}
                    </section>
                  </div>
                ) : (
                  <div className="empty-graph-card">
                    <strong>
                      {tasks.length === 0
                        ? "No tasks on this hub yet"
                        : visibility === "attention"
                          ? "All clear — nothing needs attention"
                          : "No tasks match these filters"}
                    </strong>
                    <p>
                      {tasks.length === 0
                        ? "Tasks published on the hub appear here as an interactive dependency graph."
                        : visibility === "attention"
                          ? "No scope conflicts or stuck tasks right now."
                          : "Adjust your search or scope filter to inspect other work."}
                    </p>
                    {isFiltered && (
                      <Button
                        variant="secondary"
                        size="sm"
                        onClick={() => {
                          setVisibility("all");
                          onNeedsAttentionChange(false);
                          onScopeChange("");
                        }}
                      >
                        Show all tasks ({tasks.length})
                      </Button>
                    )}
                  </div>
                )}
              </div>
            )}

            {viewMode === "graph" && filteredTasks.length > 0 && (
              <div className="graph-tools" aria-label="Canvas controls">
                <Button
                  variant="icon"
                  size="sm"
                  aria-label="Zoom in"
                  disabled={zoom >= 1.5}
                  onClick={() => setZoom((z) => Number(Math.min(1.5, z + 0.15).toFixed(2)))}
                >
                  <Plus size={15} />
                </Button>
                <Button
                  variant="icon"
                  size="sm"
                  aria-label="Zoom out"
                  disabled={zoom <= 0.4}
                  onClick={() => setZoom((z) => Number(Math.max(0.4, z - 0.15).toFixed(2)))}
                >
                  <Minus size={15} />
                </Button>
                <Button variant="icon" size="sm" aria-label="Fit graph" onClick={fitGraph}>
                  <ArrowsOut size={15} />
                </Button>
                <Button
                  variant="icon"
                  size="sm"
                  aria-label="Center selected task"
                  disabled={!active}
                  onClick={() =>
                    viewport.current?.scrollTo({
                      left: Math.max(0, (active!.x + cardWidth / 2) * scale - viewSize.width / 2),
                      top: Math.max(0, (active!.y + cardHeight / 2) * scale - viewSize.height / 2),
                      behavior: "smooth",
                    })
                  }
                >
                  <Crosshair size={15} />
                </Button>
                <output className="graph-zoom-readout" aria-label="Canvas zoom">
                  {Math.round(scale * 100)}%
                </output>
              </div>
            )}

            <button
              type="button"
              className={`presence ${runningAgentCount > 0 ? "is-active" : ""}`}
              onClick={onOpenContributors}
              aria-label={`Open participant network. ${runningAgentCount} agents working.`}
            >
              {presenceProviders.length > 0 && (
                <span className="presence-providers" aria-hidden="true">
                  {presenceProviders}
                </span>
              )}
              <span className="presence-count">
                <i /> {runningAgentCount} {runningAgentCount === 1 ? "agent" : "agents"} working
              </span>
            </button>
          </div>

          {active && (
            <div className="graph-inspector-pane">
              <TaskInspector
                task={active}
                hasSubmission={submissions.some((s) => s.task === active.id)}
                onSelect={onSelect}
                onMessages={onMessages}
                onInspectBrief={onInspectBrief}
                onOpenSubmission={onOpenSubmission}
                onOpenLoop={onOpenLoop}
              />
            </div>
          )}
        </div>
      </div>

      <div className="activity">
        {activityOpen && (
          <div className="activity-details">
            {recentActivity.length === 0 ? (
              <p className="activity-empty">No recent hub events.</p>
            ) : (
              recentActivity.map((msg) => {
                const parsed = parseMessageBody(msg);
                const ref =
                  msg.refs.find((r) => !r.startsWith("state:") && (/^SOM-|^T-/.test(r) || tasks.some((t) => t.id === r))) ??
                  msg.topic;
                const isTask = tasks.some((t) => t.id === ref) || ref === "SOM-138";
                return (
                  <div key={msg.id} className="activity-event-row">
                    <span className="mono activity-time">{msg.at}</span>
                    <StatusLabel
                      variant="chip"
                      tone={parsed.alertState === "acked" || parsed.alertState === "closed" ? "muted" : messageKindTone(msg.kind)}
                    >
                      {parsed.alertCode ?? msg.kind.replaceAll("_", " ")}
                    </StatusLabel>
                    <span className="activity-event-text">{parsed.title}</span>
                    {parsed.alertState && (
                      <StatusLabel
                        variant="chip"
                        tone={parsed.alertState === "open" ? "warning" : "muted"}
                      >
                        {parsed.alertState}
                      </StatusLabel>
                    )}
                    <button
                      type="button"
                      className="activity-ref-btn mono"
                      onClick={() => (isTask ? onOpenTask(ref) : onMessages(msg.topic))}
                    >
                      {ref}
                    </button>
                  </div>
                );
              })
            )}
            {openAlerts.map((alert) => (
              <div key={alert.key} className="activity-alert-row">
                <StatusLabel variant="pill" tone="warning">
                  {alert.code}
                </StatusLabel>
                <span>{alert.message}</span>
                {role === "Orchestrator" && (
                  <Button variant="link" onClick={() => onAckAlert(alert.key)}>
                    Acknowledge
                  </Button>
                )}
              </div>
            ))}
            <div className="activity-links">
              <button type="button" className="activity-link" onClick={() => onMessages("")}>
                Open message board <ArrowSquareOut size={14} />
              </button>
              <button type="button" className="activity-link" onClick={onOpenPipeline}>
                Open delivery pipeline <ArrowSquareOut size={14} />
              </button>
              {role === "Orchestrator" && (
                <button type="button" className="activity-link" onClick={onTogglePause}>
                  {isPaused ? <Play size={14} /> : <Pause size={14} />}
                  {isPaused ? "Resume scheduler" : "Pause scheduler"}
                </button>
              )}
            </div>
          </div>
        )}
        <div className="activity-bar">
          <button
            type="button"
            className="activity-toggle"
            aria-expanded={activityOpen}
            onClick={() => setActivityOpen(!activityOpen)}
          >
            Activity {activityOpen ? <CaretDown size={14} /> : <CaretUp size={14} />}
          </button>
          <button
            type="button"
            className="activity-latest"
            onClick={() =>
              onMessages(
                latestMessage?.topic.startsWith("review/")
                  ? `${latestMessage.topic.split("/").slice(0, 2).join("/")}/*`
                  : latestMessage?.topic ?? "",
              )
            }
          >
            <i />
            {latestMessage && latestParsed ? (
              <>
                <StatusLabel
                  variant="chip"
                  tone={latestParsed.alertState === "acked" ? "muted" : messageKindTone(latestMessage.kind)}
                >
                  {latestParsed.alertCode ?? latestMessage.kind}
                </StatusLabel>
                <span className="activity-latest-text">{latestParsed.title}</span>
                {latestRef && <span className="activity-latest-ref mono">{latestRef}</span>}
                <span className="activity-latest-time mono">{latestMessage.at}</span>
              </>
            ) : (
              <span>No recent events</span>
            )}
          </button>
          <div className="activity-meta-badges">
            {scheduler.safeMode && (
              <StatusLabel variant="pill" tone="danger">
                Safe mode
              </StatusLabel>
            )}
            {isPaused && (
              <StatusLabel variant="pill" tone="warning">
                Scheduler paused
              </StatusLabel>
            )}
            {openAlerts.length > 0 && (
              <StatusLabel variant="pill" tone="warning">
                {openAlerts.length} {openAlerts.length === 1 ? "alert" : "alerts"}
              </StatusLabel>
            )}
          </div>
        </div>
      </div>
    </>
  );
}
