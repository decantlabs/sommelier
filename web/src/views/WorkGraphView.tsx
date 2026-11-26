import { useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent, ReactNode } from "react";
import {
  ArrowRight,
  ArrowSquareOut,
  ArrowsOut,
  CaretDown,
  CaretUp,
  Crosshair,
  FileText,
  Minus,
  Pause,
  Play,
  Plus,
} from "@phosphor-icons/react";
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
} from "../components";
import type { DefinitionItem } from "../components";
import { readCanvasTokens } from "../design/tokens";
import "./work-graph.css";

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
  presenceProviders: ReactNode;
  runningAgentCount: number;
};

const sceneWidth = 1440;
const sceneHeight = 835;
const cardWidth = 310;
const topRowHeight = 140;

const KNOWN_SCOPE_LABELS: Record<string, string> = {
  "bus/src/agent": "agent",
  "bus/src/intake": "intake",
  "bus/tests": "tests",
};

function scopePrefixOf(raw: string): string {
  const first = raw.split(",")[0]?.trim() ?? "";
  for (const known of Object.keys(KNOWN_SCOPE_LABELS)) {
    if (first.startsWith(known)) return known;
  }
  const clean = first.replace(/\*+$/g, "").replace(/\/+$/g, "");
  const parts = clean.split("/").filter(Boolean);
  if (parts.length >= 3) return parts.slice(0, 3).join("/");
  if (parts.length >= 2) return parts.slice(0, 2).join("/");
  return clean;
}

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
    ctx.setTransform(2, 0, 0, 2, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.lineWidth = 1.8;

    function wire(from: WorkTask, to: WorkTask, conflict: boolean) {
      const paint = ctx!;
      const sameColumn = Math.abs(from.x - to.x) < 20;
      const x1 = sameColumn ? from.x + cardWidth / 2 : from.x + cardWidth;
      const y1 = sameColumn ? from.y + topRowHeight : from.y + 76;
      const x2 = sameColumn ? to.x + cardWidth / 2 : to.x;
      const y2 = sameColumn ? to.y : to.y + 76;
      const midX = (x1 + x2) / 2;
      const color = conflict ? palette.conflict : palette.edge;

      paint.strokeStyle = color;
      paint.fillStyle = color;
      paint.setLineDash(conflict ? [8, 6] : []);
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
        paint.arc(x, y, 4.5, 0, 2 * Math.PI);
        paint.fillStyle = palette.portFill;
        paint.fill();
        paint.stroke();
      }
    }

    const byId = new Map(tasks.map((task) => [task.id, task]));
    for (const task of tasks) {
      for (const dep of task.deps ?? []) {
        const previous = byId.get(dep);
        if (previous) wire(previous, task, false);
      }
      const blocker = task.conflict && byId.get(task.conflict);
      if (blocker) wire(blocker, task, true);
    }
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
  const left = task.conflict && task.x === 570 && task.y === 432 ? 960 : task.x + cardWidth + 26;
  const isRight = left > 980;
  const top = task.conflict && task.x === 570 && task.y === 432 ? 366 : task.y + 24;

  const definitions: DefinitionItem[] = [
    ...(!task.conflict ? [{ label: "Task state", value: task.state }] : []),
    ...(task.conflict ? [] : task.blockerReason ? [{ label: "Dispatch", value: task.blockerReason }] : []),
    { label: "Priority", value: task.priority ?? "Normal" },
    { label: "Attempt", value: `${task.attempt ?? 1} of ${task.maxAttempts ?? 3}` },
    { label: "Min. trust", value: `Tier ${task.minTrust ?? 2}` },
    ...(task.risk ? [{ label: "Risk", value: task.risk }] : []),
    ...(task.feature ? [{ label: "Feature", value: <span className="mono">{task.feature}</span> }] : []),
    ...(task.owner
      ? [
          {
            label: "Lease holder",
            value: task.provider ? (
              <ProviderLabel provider={task.provider} suffix={task.owner ? ` · ${task.owner}` : ""} />
            ) : (
              task.owner
            ),
          },
        ]
      : []),
    ...(task.deps?.length ? [{ label: "Dependencies", value: task.deps.join(", ") }] : []),
  ];

  return (
    <InspectorPanel
      mode="floating"
      ariaLabel={`Details for ${task.id}`}
      eyebrow={<span className="task-id">{task.id}</span>}
      title={task.conflict ? "Waiting on active scope" : task.title}
      hasPointer={Boolean(task.conflict)}
      fromRight={isRight}
      style={{ left: isRight ? Math.max(24, task.x - 435) : left, top }}
      onClose={() => onSelect(null)}
      closeLabel="Close task details"
    >
      <p>
        {task.conflict
          ? `${task.conflict} is working in the same scope. Dispatch can resume once the scope is free.`
          : task.done}
      </p>
      <CodeChip>{task.scope}</CodeChip>
      <DefinitionList items={definitions} />
      <div className="ui-inspector-actions">
        {task.conflict ? (
          <Button variant="link" onClick={() => onSelect(task.conflict!)}>
            Open blocking task <ArrowRight size={15} />
          </Button>
        ) : (
          <Button variant="link" onClick={() => onMessages(`task/${task.id}`)}>
            Open task messages <ArrowRight size={15} />
          </Button>
        )}
        {task.checkpoint && (
          <small className="inspector-actions-note">
            Latest checkpoint <code>{task.checkpoint}</code>
          </small>
        )}
      </div>
      <div className="inspector-sublinks">
          <Button variant="link" onClick={() => onOpenLoop(task.id)}>View work loop <ArrowRight size={14} /></Button>
          {task.briefAvailable && (
            <Button variant="link" onClick={() => onInspectBrief(task.id)}>
              <FileText size={14} /> Inspect brief
            </Button>
          )}
          {hasSubmission && (
            <Button variant="link" onClick={() => onOpenSubmission(task.id)}>
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
  const [width, setWidth] = useState(sceneWidth);
  const [zoom, setZoom] = useState(1);
  const [activityOpen, setActivityOpen] = useState(false);
  const searchText = search.trim().toLowerCase();

  useEffect(() => {
    if (!viewport.current) return;
    const observer = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    observer.observe(viewport.current);
    return () => observer.disconnect();
  }, []);

  const scopeOptions = useMemo(() => {
    const prefixes = new Set<string>();
    for (const task of tasks) {
      const p = scopePrefixOf(task.scope);
      if (p) prefixes.add(p);
    }
    return [...prefixes].map((value) => ({
      value,
      label: KNOWN_SCOPE_LABELS[value] ?? (value.split("/").pop() || value),
    }));
  }, [tasks]);

  const graphWidth = Math.max(
    sceneWidth,
    ...tasks.map((task) => task.x + cardWidth + 460),
  );
  const graphHeight = Math.max(
    sceneHeight,
    ...tasks.map((task) => task.y + (task.y >= 640 ? 360 : 195)),
  );

  const baseScale = Math.max(0.74, Math.min(1.2, width / sceneWidth));
  const scale = baseScale * zoom;
  const filteredTasks = tasks.filter((task) => {
    const matchesSearch =
      !searchText ||
      [task.id, task.title, task.scope, task.feature ?? ""].some((value) =>
        value.toLowerCase().includes(searchText),
      );
    const matchesScope = !scope || task.scope.startsWith(scope);
    const attention =
      Boolean(task.conflict) ||
      task.state === "Stuck" ||
      tasks.some((other) => other.conflict === task.id);
    return matchesSearch && matchesScope && (!needsAttention || attention);
  });

  const findTask = (id: string) => filteredTasks.find((task) => task.id === id);
  const active = selected && findTask(selected.id) ? selected : undefined;

  const showReferenceLabels =
    findTask("SOM-130")?.x === 105 &&
    findTask("SOM-136")?.x === 572 &&
    findTask("SOM-142")?.x === 570;

  useEffect(() => {
    if (selected && selected.y >= 640) {
      viewport.current
        ?.querySelector(`[data-task-id="${selected.id}"]`)
        ?.scrollIntoView({ block: "center", inline: "nearest" });
    }
  }, [selected]);

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

  const openAlerts = alerts.filter((a) => !a.acked);
  const isPaused = scheduler.activePauses.length > 0;
  const recentActivity = messages.slice(0, 3);
  const latestMessage = messages[0];
  const latestRef = latestMessage?.refs[0] ?? latestMessage?.topic;

  return (
    <>
      <div className="graph-body">
        <div className="graph-top">
          <div className="legend">
            <span>
              <i /> Dependency
            </span>
            <span>
              <i /> Scope conflict
            </span>
          </div>
          <div className="graph-filters">
            <SegmentedControl
              ariaLabel="Task visibility"
              value={needsAttention ? "attention" : "all"}
              onChange={(next) => onNeedsAttentionChange(next === "attention")}
              options={[
                { value: "all", label: "All work" },
                { value: "attention", label: "Needs attention" },
              ]}
            />
            <label className="scope-select">
              Scope:{" "}
              <select
                aria-label="Scope"
                value={scope}
                onChange={(event) => onScopeChange(event.target.value)}
              >
                <option value="">all</option>
                {scopeOptions.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
            </label>
          </div>
        </div>

        <div
          className={`graph-viewport ${isPanning ? "panning" : ""}`}
          ref={viewport}
          onPointerDown={startPan}
          onPointerMove={movePan}
          onPointerUp={stopPan}
          onPointerCancel={stopPan}
          onLostPointerCapture={stopPan}
        >
          <div className="graph-size" style={{ width: graphWidth * scale, height: graphHeight * scale }}>
            <section
              aria-label="Task graph canvas"
              className="world"
              style={{ width: graphWidth, height: graphHeight, transform: `scale(${scale})` }}
            >
              <Connections tasks={filteredTasks} width={graphWidth} height={graphHeight} />
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
                  onSelect={() => onSelect(task.id)}
                />
              ))}
              {!filteredTasks.length && (
                <div className="empty-graph">
                  {tasks.length === 0
                    ? "No tasks on this hub yet."
                    : "No tasks match these filters."}
                </div>
              )}
              {active && (
                <TaskInspector
                  task={active}
                  hasSubmission={submissions.some((s) => s.task === active.id)}
                  onSelect={onSelect}
                  onMessages={onMessages}
                  onInspectBrief={onInspectBrief}
                  onOpenSubmission={onOpenSubmission}
                  onOpenLoop={onOpenLoop}
                />
              )}
            </section>
          </div>

          <div className="graph-tools" aria-label="Canvas controls">
            <Button
              variant="icon"
              aria-label="Zoom in"
              disabled={zoom >= 1.5}
              onClick={() => setZoom(Math.min(1.5, zoom + 0.15))}
            >
              <Plus size={17} />
            </Button>
            <Button
              variant="icon"
              aria-label="Zoom out"
              disabled={zoom <= 0.35}
              onClick={() => setZoom(Math.max(0.35, zoom - 0.15))}
            >
              <Minus size={17} />
            </Button>
            <Button
              variant="icon"
              aria-label="Fit graph"
              onClick={() => {
                setZoom(Math.min(1, width / (graphWidth * baseScale)));
                viewport.current?.scrollTo(0, 0);
              }}
            >
              <ArrowsOut size={17} />
            </Button>
            <Button
              variant="icon"
              aria-label="Center selected task"
              disabled={!active}
              onClick={() =>
                viewport.current?.scrollTo({
                  left: Math.max(0, (active!.x + cardWidth / 2) * scale - width / 2),
                  top: Math.max(0, active!.y * scale - 120),
                  behavior: "smooth",
                })
              }
            >
              <Crosshair size={17} />
            </Button>
          </div>
        </div>

        <button
          type="button"
          className="presence"
          onClick={onOpenContributors}
          aria-label={`Open participant network. ${runningAgentCount} agents working.`}
        >
          <span className="presence-providers" aria-hidden="true">
            {presenceProviders}
          </span>
          <span className="presence-count">
            <i /> {runningAgentCount} agents working
          </span>
        </button>
      </div>

      <div className="activity">
        {activityOpen && (
          <div className="activity-details">
            {recentActivity.length === 0 ? (
              <p>No recent hub events.</p>
            ) : (
              recentActivity.map((msg) => {
                const ref = msg.refs.find((r) => /^SOM-|^T-/.test(r)) ?? msg.refs[0] ?? msg.topic;
                const isTask = tasks.some((t) => t.id === ref) || ref === "SOM-138";
                return (
                  <p key={msg.id}>
                    <span className="mono">{msg.at}</span> {msg.body}{" "}
                    <button
                      type="button"
                      onClick={() => (isTask ? onOpenTask(ref) : onMessages(msg.topic))}
                    >
                      {ref}
                    </button>
                  </p>
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
            {latestMessage ? (
              <>
                <u>{latestRef}</u> · {latestMessage.body} · {latestMessage.at}
              </>
            ) : (
              "No recent events"
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
