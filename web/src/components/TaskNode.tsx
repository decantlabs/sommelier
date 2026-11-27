import { CheckCircle, Circle, Code, GitBranch, LinkSimple, Prohibit, Robot, Warning, WarningCircle } from "@phosphor-icons/react";
import type { WorkTask } from "../api/types";
import { Avatar } from "./Avatar";
import { ProviderMark } from "./ProviderMark";
import { StatusLabel, taskStateTone } from "./StatusLabel";

function isHeartbeatStale(heartbeat?: string): boolean {
  if (!heartbeat) return false;
  if (heartbeat.includes("h ago") || heartbeat.includes("d ago")) return true;
  const minMatch = /^(\d+)m ago$/.exec(heartbeat);
  if (minMatch && Number(minMatch[1]) >= 5) return true;
  return false;
}

export function TaskNode({
  task,
  selected,
  positioned = true,
  onSelect,
}: {
  task: WorkTask;
  selected: boolean;
  positioned?: boolean;
  onSelect: () => void;
}) {
  const isMuted = task.state === "Cancelled" || task.state === "Closed";
  const healthy = task.state === "Running" || task.state === "Closed" || task.state === "Landing";
  const StatusIcon =
    task.state === "Closed"
      ? CheckCircle
      : task.state === "Cancelled"
        ? Prohibit
        : task.state === "Stuck"
          ? WarningCircle
          : Circle;
  const hasBlocker = Boolean(task.conflict || task.blockerReason);
  const blockerText = task.conflict ? `Waiting on ${task.conflict}` : task.blockerReason;
  const staleHeartbeat = isHeartbeatStale(task.heartbeat);
  const showPriority = task.priority === "Blocker" || task.priority === "High";

  return (
    <button
      type="button"
      className={[
        "task-node",
        positioned ? "is-positioned" : "is-grid-card",
        selected ? "selected" : "",
        hasBlocker ? "has-conflict" : "",
        isMuted ? "is-muted" : "",
      ]
        .filter(Boolean)
        .join(" ")}
      style={positioned ? { left: task.x, top: task.y } : undefined}
      onClick={onSelect}
      data-task-id={task.id}
      aria-pressed={selected}
      aria-label={`${task.id} ${task.title}, ${task.state}${blockerText ? `, ${blockerText.toLowerCase()}` : ""}`}
    >
      <div className="task-node-head">
        <span className="task-id" title={task.id}>{task.id}</span>
        <div className="task-node-badges">
          {showPriority && (
            <StatusLabel
              variant="chip"
              tone={task.priority === "Blocker" ? "danger" : "warning"}
            >
              {task.priority}
            </StatusLabel>
          )}
          <StatusLabel
            variant="pill"
            tone={taskStateTone(task.state)}
            icon={<StatusIcon size={12} weight={healthy ? "fill" : "bold"} />}
          >
            {task.state}
          </StatusLabel>
        </div>
      </div>

      <strong className="task-node-title" title={task.title}>
        {task.title}
      </strong>

      {hasBlocker ? (
        <span className="task-node-blocker">
          <Warning size={15} weight="fill" /> {blockerText}
        </span>
      ) : null}

      <div className="task-node-footer">
        {task.scope ? (
          <span className="task-node-scope mono" title={task.scope}>
            <Code size={13} aria-hidden="true" />
            <span>{task.scope}</span>
          </span>
        ) : null}

        <span className="task-node-meta">
          {task.owner ? (
            <>
              {task.provider ? <ProviderMark provider={task.provider} size={14} /> : <Robot size={14} />}
              <span className="task-node-owner">
                {task.owner}
                {task.provider ? ` / ${task.provider}` : ""}
              </span>
              {task.heartbeat && (
                <small className={staleHeartbeat ? "is-stale" : undefined}>
                  {staleHeartbeat ? `Last seen ${task.heartbeat}` : `Seen ${task.heartbeat}`}
                </small>
              )}
            </>
          ) : task.sha ? (
            <>
              <GitBranch size={14} />
              <span className="mono">{task.sha}</span>
              {task.people && (
                <span className="task-node-people">
                  {task.people.map((person) => (
                    <Avatar key={person} initials={person} person={person} size="xs" />
                  ))}
                </span>
              )}
            </>
          ) : task.deps?.length ? (
            <>
              <LinkSimple size={14} />
              <span>Depends on {task.deps.join(", ")}</span>
            </>
          ) : task.state === "Draft" ? (
            "Awaiting publication"
          ) : task.state === "Cancelled" ? (
            "Cancelled"
          ) : task.state === "Closed" ? (
            "Completed"
          ) : (
            "Ready for dispatch"
          )}
        </span>
      </div>
    </button>
  );
}
