import { useState } from "react";
import { ArrowCounterClockwise, ArrowRight, Circle, Code, GitCommit, Pulse } from "@phosphor-icons/react";
import type { SubmissionItem, WorkAttempt, WorkTask } from "../api/types";
import { Button, CodeChip, DefinitionList, FilterBar, FilterField, InspectorPanel, StatusLabel } from "../components";
import { deliveryStages, stageStateIcons, stageStateLabels, stageStatuses, type StageStatus } from "./PipelineView";
import "./loops.css";

type LoopState = "Working" | "Waiting" | "Blocked" | "Complete" | "Stopped";
type Loop = { task: WorkTask; submission?: SubmissionItem; attempts: WorkAttempt[]; state: LoopState; reason: string };
const tones = { Working: "accent", Waiting: "warning", Blocked: "danger", Complete: "good", Stopped: "muted" } as const;

function describeLoop(task: WorkTask, submission?: SubmissionItem): Pick<Loop, "state" | "reason"> {
  if (task.state === "Cancelled") return { state: "Stopped", reason: "The task was cancelled." };
  if (task.state === "Closed") return { state: "Complete", reason: "The hub closed this task." };
  if (task.state === "Stuck") {
    const exhausted = task.attempt !== undefined && task.maxAttempts !== undefined && task.attempt >= task.maxAttempts;
    return { state: "Blocked", reason: exhausted ? "The recorded attempt limit has been reached." : "The task is marked stuck and needs attention." };
  }
  if (submission?.state.endsWith("_failed") || submission?.state === "rejected") {
    return { state: "Blocked", reason: `Submission ${submission.state.replaceAll("_", " ")}.` };
  }
  if (task.conflict || task.blockerReason) {
    return { state: "Waiting", reason: task.conflict ? `Dispatch waits for ${task.conflict} to release the overlapping scope.` : task.blockerReason! };
  }
  if (submission?.state === "reviewing") return { state: "Waiting", reason: submission.reviewSummary ?? "Waiting for the required independent reviews." };
  if (submission?.state === "verifying") return { state: "Working", reason: "Clean verification is in progress." };
  if (submission?.state === "approved" || task.state === "Landing") return { state: "Working", reason: "The change is in the landing process." };
  if (submission?.state === "landed") return { state: "Working", reason: "The submission landed. Task closure is tracked separately." };
  if (task.state === "Running" || task.state === "Leased") return { state: "Working", reason: task.owner ? `Work is leased to ${task.owner}.` : "Work is in progress." };
  if (task.state === "Delivered") return { state: "Waiting", reason: "Work was delivered. Awaiting the next pipeline step." };
  if (task.state === "Draft") return { state: "Waiting", reason: "The task is still a draft." };
  if (task.state === "Offered") return { state: "Waiting", reason: "Waiting for an offered assignment to be accepted." };
  return { state: "Waiting", reason: "The task is ready for scheduling." };
}

function LoopTrace({ loop }: { loop: Loop }) {
  const { task, submission } = loop;
  const work: StageStatus = task.state === "Stuck" ? "fail"
    : submission || ["Delivered", "Landing", "Closed"].includes(task.state) ? "pass"
    : ["Running", "Leased"].includes(task.state) ? "active" : "idle";
  const following: StageStatus[] = submission ? stageStatuses(submission) : ["idle", "idle", "idle", "idle"];
  return (
    <ol className="loop-stages" aria-label="Observed delivery stages">
      {[{ label: "Work" }, ...deliveryStages].map(({ label }, index) => {
        const state = [work, ...following][index];
        const Icon = stageStateIcons[state] ?? Circle;
        return (
          <li key={label} data-stage={state} aria-current={state === "active" ? "step" : undefined}>
            <span className="loop-stage-mark"><Icon size={18} aria-hidden="true" /></span>
            <strong>{label}</strong>
            <small>{index === 0 && state === "idle" && ["Ready", "Offered"].includes(task.state) ? "Waiting for dispatch" : stageStateLabels[state]}</small>
          </li>
        );
      })}
    </ol>
  );
}

export default function LoopsView({ tasks, submissions, attempts, initialTaskId, onTask, onPipeline }: {
  tasks: WorkTask[];
  submissions: SubmissionItem[];
  attempts: WorkAttempt[];
  initialTaskId: string | null;
  onTask: (id: string) => void;
  onPipeline: (id: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState("");
  const [selected, setSelected] = useState<string | null>(initialTaskId);
  const [limit, setLimit] = useState(16);
  const attemptsByTask = new Map<string, WorkAttempt[]>();
  for (const attempt of attempts) {
    const group = attemptsByTask.get(attempt.taskId) ?? [];
    group.push(attempt);
    attemptsByTask.set(attempt.taskId, group);
  }
  const submissionsByTask = new Map<string, SubmissionItem>();
  for (const submission of submissions) {
    const previous = submissionsByTask.get(submission.task);
    if (!previous || submission.attempt > previous.attempt) submissionsByTask.set(submission.task, submission);
  }
  const rows: Loop[] = tasks.map((task) => {
    const latest = submissionsByTask.get(task.id);
    const submission = latest?.attempt === (task.attempt ?? 1) ? latest : undefined;
    return { task, submission, attempts: attemptsByTask.get(task.id) ?? [], ...describeLoop(task, submission) };
  });
  const search = query.trim().toLowerCase();
  const visible = rows.filter((loop) =>
    (!status || loop.state === status) && (!search || `${loop.task.id} ${loop.task.title}`.toLowerCase().includes(search)));
  const active = visible.find(({ task }) => task.id === selected)
    ?? visible.find(({ task }) => (task.attempt ?? 1) > 1) ?? visible[0];
  const history = active ? [...active.attempts].sort((a, b) => b.number - a.number) : [];
  const previous = active ? history.find(({ number }) => number < (active.task.attempt ?? 1)) : undefined;
  const latestAttempt = active ? history.find(({ number }) => number === (active.task.attempt ?? 1)) : undefined;
  const checkpoint = latestAttempt?.checkpoint ?? active?.task.checkpoint;
  const isRetry = active && previous && (previous.endReason || previous.leaseState === "expired");
  const ledger = active && !visible.slice(0, limit).includes(active)
    ? [...visible.slice(0, limit - 1), active] : visible.slice(0, limit);

  return (
    <section className="pl-view loop-view" aria-labelledby="loop-title">
      <header className="pl-header">
        <div><h1 id="loop-title">Work loops</h1><p>Follow attempts, checkpoints and delivery evidence for each task.</p></div>
        <StatusLabel variant="dot" tone="accent" icon={<Pulse size={15} />}>{rows.filter(({ state }) => state === "Working").length} working</StatusLabel>
      </header>
      <FilterBar ariaLabel="Loop filters">
        <FilterField label="Find problem"><input value={query} onChange={(event) => { setQuery(event.target.value); setLimit(16); }} placeholder="Task ID or title" /></FilterField>
        <FilterField label="State"><select value={status} onChange={(event) => { setStatus(event.target.value); setLimit(16); }}>
          <option value="">All states</option>{Object.keys(tones).map((state) => <option key={state}>{state}</option>)}
        </select></FilterField>
      </FilterBar>

      <div className="loop-workspace">
        <section className="loop-queue" aria-label="Work loops">
          <div className="pl-list-head"><span>Tracked problems</span><StatusLabel variant="chip">{visible.length}</StatusLabel></div>
          {ledger.length ? <ul>{ledger.map((loop) => (
            <li key={loop.task.id}>
              <button type="button" aria-pressed={active?.task.id === loop.task.id} onClick={() => setSelected(loop.task.id)}>
                <span className="loop-row-title"><code>{loop.task.id}</code><strong>{loop.task.title}</strong></span>
                <StatusLabel variant="dot" tone={tones[loop.state]}>{loop.state}</StatusLabel>
              </button>
            </li>
          ))}</ul> : <p className="pl-empty">No work loops match these filters.</p>}
          {visible.length > limit && <div className="loop-more"><Button size="sm" onClick={() => setLimit(limit + 16)}>Show more</Button></div>}
        </section>

        {active && (
          <div className="loop-detail">
            <div className="loop-diagram">
              <div className="loop-diagram-head"><code>{active.task.id}</code><StatusLabel variant="pill" tone={tones[active.state]}>{active.state}</StatusLabel></div>
              <h2>{active.task.title}</h2>
              <p>{active.reason}</p>
              <LoopTrace loop={active} />
              {isRetry && <p className="loop-feedback"><ArrowCounterClockwise size={19} aria-hidden="true" /> {active.task.state === "Ready" || active.task.state === "Offered" ? "Retry queued after attempt" : "Retried after attempt"} {previous.number}{previous.endReason ? ` · ${previous.endReason.replaceAll("_", " ")}` : ""}</p>}
              <div className="loop-diagram-actions">
                <Button variant="link" onClick={() => onTask(active.task.id)}>Open in Work Graph <ArrowRight size={14} /></Button>
                {active.submission && <Button variant="link" onClick={() => onPipeline(active.task.id)}>Open in Pipeline <ArrowRight size={14} /></Button>}
              </div>
            </div>
            <div className="loop-sidecars">
              <InspectorPanel ariaLabel="Completion criteria" eyebrow={<><Code size={15} /> Completion criteria</>}>
                <p className="loop-done">{active.task.done}</p>
                <DefinitionList items={[
                  { label: "Task state", value: active.task.state },
                  { label: "Attempt", value: `${active.task.attempt ?? 1}${active.task.maxAttempts ? ` / ${active.task.maxAttempts}` : ""}` },
                  ...(checkpoint ? [{ label: "Checkpoint", value: <CodeChip small>{checkpoint}</CodeChip> }] : []),
                  ...(active.task.scope ? [{ label: "Scope", value: <CodeChip small>{active.task.scope}</CodeChip> }] : []),
                ]} />
              </InspectorPanel>
              <InspectorPanel ariaLabel="Attempt history" eyebrow={<><GitCommit size={15} /> Attempt history</>}>
                {history.length ? <ol className="loop-attempts">{history.map((attempt) => (
                  <li key={attempt.number}>
                    <strong>Attempt {attempt.number}</strong>
                    <small>{[attempt.leaseState, attempt.owner, attempt.worker && attempt.worker !== attempt.owner ? attempt.worker : undefined].filter(Boolean).join(" · ") || "Checkpoint recorded"}</small>
                    {attempt.endReason && <small>{attempt.endReason.replaceAll("_", " ")}</small>}
                    {attempt.checkpoint && <CodeChip small>{attempt.checkpoint}</CodeChip>}
                  </li>
                ))}</ol> : <p className="loop-done">No earlier lease or checkpoint history in this snapshot.</p>}
              </InspectorPanel>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}
