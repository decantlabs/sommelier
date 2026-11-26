import { CheckCircle, Circle, Code, GitBranch, LinkSimple, Robot, Warning } from "@phosphor-icons/react";
import type { WorkTask } from "../api/types";
import { Avatar } from "./Avatar";
import { ProviderMark } from "./ProviderMark";
import { StatusLabel, taskStateTone } from "./StatusLabel";

export function TaskNode({
  task,
  selected,
  onSelect,
}: {
  task: WorkTask;
  selected: boolean;
  onSelect: () => void;
}) {
  const healthy = task.state === "Running" || task.state === "Closed";
  const StatusIcon = task.state === "Closed" ? CheckCircle : Circle;
  const hasBlocker = Boolean(task.conflict || task.blockerReason);
  const blockerText = task.conflict ? "Waiting on active scope" : task.blockerReason;

  return (
    <button
      type="button"
      className={`task-node ${selected ? "selected" : ""} ${hasBlocker ? "has-conflict" : ""}`.trim()}
      style={{ left: task.x, top: task.y }}
      onClick={onSelect}
      data-task-id={task.id}
      aria-pressed={selected}
      aria-label={`${task.id} ${task.title}, ${task.state}${blockerText ? `, ${blockerText.toLowerCase()}` : ""}`}
    >
      <span className="task-id">{task.id}</span>
      <strong>{task.title}</strong>
      <StatusLabel
        variant="inline"
        tone={taskStateTone(task.state)}
        className="task-node-state"
        icon={<StatusIcon size={17} weight={healthy ? "fill" : "bold"} />}
      >
        {task.state}
      </StatusLabel>
      {hasBlocker ? (
        <>
          <span className="task-node-blocker">
            <Warning size={17} /> {blockerText}
          </span>
          <span className="task-node-meta mono">
            <Code size={16} /> {task.scope}
          </span>
        </>
      ) : (
        <span className="task-node-meta">
          {task.owner ? (
            <>
              {task.provider ? <ProviderMark provider={task.provider} /> : <Robot size={16} />} {task.owner}
              {task.provider && ` / ${task.provider}`}
              {task.heartbeat && <small>Heartbeat {task.heartbeat}</small>}
            </>
          ) : task.sha ? (
            <>
              <GitBranch size={16} />
              <span className="mono">{task.sha}</span>
              {task.people && (
                <span className="task-node-people">
                  {task.people.map((person) => (
                    <Avatar key={person} initials={person} size="sm" />
                  ))}
                </span>
              )}
            </>
          ) : task.deps?.length ? (
            <>
              <LinkSimple size={16} /> Depends on {task.deps.join(", ")}
            </>
          ) : task.state === "Draft" ? (
            "Awaiting publication"
          ) : (
            "No dependency blockers"
          )}
        </span>
      )}
    </button>
  );
}
