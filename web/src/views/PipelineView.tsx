import { useEffect, useMemo, useState } from "react";
import { ArrowRight, CheckCircle, Circle, Clock, GitBranch, GitCommit, Scales, ShieldCheck, WarningCircle, X } from "@phosphor-icons/react";
import type { BatchItem, LandingItem, ReviewItem, SubmissionItem } from "../api/types";
import {
  Button,
  CodeChip,
  DefinitionList,
  FilterBar,
  FilterField,
  InspectorPanel,
  SegmentedControl,
  StatusLabel,
  submissionStateTone,
} from "../components";
import "./pipeline.css";

type PipelineSection = "submissions" | "reviews" | "landing";

export const deliveryStages = [
  { label: "Gate", description: "Scope and policy", icon: ShieldCheck, states: ["received", "gated", "gate_failed"] },
  { label: "Verify", description: "Clean build", icon: GitBranch, states: ["verifying", "verified", "verify_failed"] },
  { label: "Review", description: "Independent checks", icon: Scales, states: ["reviewing", "review_failed"] },
  { label: "Land", description: "Approved changes", icon: GitCommit, states: ["approved", "landed"] },
] as const;

export type StageStatus = "pass" | "active" | "fail" | "idle";

export function stageStatuses(sub: SubmissionItem): StageStatus[] {
  if (sub.state === "rejected") return ["idle", "idle", "idle", "idle"];
  const stage = deliveryStages.findIndex(({ states }) => (states as readonly string[]).includes(sub.state));
  const finished = ["gated", "verified", "landed"].includes(sub.state);
  return deliveryStages.map((_, index) => {
    if (index < stage || (index === stage && finished)) return "pass";
    if (index > stage) return "idle";
    return sub.state.endsWith("_failed") ? "fail" : "active";
  });
}

export const stageStateLabels = { pass: "Passed", active: "Current stage", fail: "Failed", idle: "Not reached" };
export const stageStateIcons = { pass: CheckCircle, active: Clock, fail: WarningCircle, idle: Circle };

function SubmissionTrace({ submission }: { submission: SubmissionItem }) {
  if (submission.state === "rejected") return null;
  return (
    <ol className="pl-trace" aria-label="Submission progress">
      {stageStatuses(submission).map((state, index) => {
        const Icon = stageStateIcons[state];
        return (
          <li key={deliveryStages[index].label} className={`is-${state}`} aria-current={state === "active" ? "step" : undefined}>
            <Icon size={16} aria-hidden="true" />
            <strong>{deliveryStages[index].label}</strong>
            <span>{stageStateLabels[state]}</span>
          </li>
        );
      })}
    </ol>
  );
}

export default function PipelineView({
  submissions,
  reviews,
  landing,
  batches,
  initialTaskFilter,
  onClearTaskFilter,
  onOpenTask,
  onMessages,
}: {
  submissions: SubmissionItem[];
  reviews: ReviewItem[];
  landing: LandingItem[];
  batches: BatchItem[];
  initialTaskFilter: string | null;
  onClearTaskFilter: () => void;
  onOpenTask: (taskId: string) => void;
  onMessages: (topic: string) => void;
}) {
  const [section, setSection] = useState<PipelineSection>("submissions");
  const pageSize = 20;
  const [visibleLimit, setVisibleLimit] = useState(pageSize);
  const [stateFilter, setStateFilter] = useState("");
  const [stageFilter, setStageFilter] = useState<number | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(
    () =>
      submissions.find((s) => s.task === initialTaskFilter)?.sha256 ??
      submissions[0]?.sha256 ??
      null,
  );

  useEffect(() => {
    if (!initialTaskFilter) return;
    setSection("submissions");
    setStageFilter(null);
    setStateFilter("");
    setSelectedKey(null);
  }, [initialTaskFilter]);

  const visibleSubmissions = useMemo(
    () =>
      submissions.filter(
        (item) =>
          (!stateFilter || item.state === stateFilter) &&
          (stageFilter === null || (deliveryStages[stageFilter].states as readonly string[]).includes(item.state)) &&
          (!initialTaskFilter || item.task === initialTaskFilter),
      ),
    [initialTaskFilter, stageFilter, stateFilter, submissions],
  );

  const activeSubmission =
    section === "submissions"
      ? visibleSubmissions.find((s) => s.sha256 === selectedKey) ?? visibleSubmissions[0]
      : undefined;
  const activeReview =
    section === "reviews"
      ? reviews.find((r) => `${r.task}#${r.n}` === selectedKey) ?? reviews[0]
      : undefined;
  const activeLanding =
    section === "landing"
      ? landing.find((l) => l.refName === selectedKey) ?? landing[0]
      : undefined;

  const hasDetail = Boolean(activeSubmission || activeReview || activeLanding);
  const entryCount = section === "submissions" ? visibleSubmissions.length : section === "reviews" ? reviews.length : landing.length;
  const submissionStates = useMemo(
    () => [...new Set(submissions.map((s) => s.state))].sort(),
    [submissions],
  );

  return (
    <section className="pl-view" aria-labelledby="pl-title">
      <div className="pl-header">
        <div>
          <h1 id="pl-title">Delivery pipeline</h1>
          <p>Follow submissions from intake to landing.</p>
        </div>
        <SegmentedControl
          ariaLabel="Pipeline stage"
          value={section}
          onChange={(next) => {
            setSection(next);
            setVisibleLimit(pageSize);
            setSelectedKey(null);
          }}
          options={[
            { value: "submissions", label: `Submissions (${submissions.length})` },
            { value: "reviews", label: `Reviews (${reviews.length})` },
            { value: "landing", label: `Landing (${landing.length})` },
          ]}
        />
      </div>

      {section === "submissions" && (
        <div className="pl-stage-strip" role="group" aria-label="Filter by delivery stage">
          {deliveryStages.map(({ label, description, icon: Icon, states }, index) => {
            const count = submissions.filter(({ state }) => (states as readonly string[]).includes(state)).length;
            return (
              <button key={label} type="button" aria-pressed={stageFilter === index}
                onClick={() => { setStageFilter(stageFilter === index ? null : index); setStateFilter(""); setVisibleLimit(pageSize); }}>
                <span className="pl-stage-icon"><Icon size={17} aria-hidden="true" /></span>
                <span><strong>{label}</strong><small>{description}</small></span>
                <span className="pl-stage-total mono">{count}</span>
              </button>
            );
          })}
        </div>
      )}

      {section === "submissions" && (
        <FilterBar ariaLabel="Submission filters">
          <FilterField label="State">
            <select value={stateFilter} onChange={(e) => { setStateFilter(e.target.value); setStageFilter(null); setVisibleLimit(pageSize); }}>
              <option value="">All states</option>
              {submissionStates.map((st) => (
                <option key={st} value={st}>
                  {st.replaceAll("_", " ")}
                </option>
              ))}
            </select>
          </FilterField>
          {initialTaskFilter && (
            <Button variant="secondary" size="sm" onClick={onClearTaskFilter}>
              Task: {initialTaskFilter} <X size={13} />
            </Button>
          )}
          {(stageFilter !== null || stateFilter || initialTaskFilter) && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setStateFilter("");
                setStageFilter(null);
                onClearTaskFilter();
              }}
            >
              Clear filters
            </Button>
          )}
        </FilterBar>
      )}

      <div className="pl-content" data-detail={hasDetail}>
        <div className="pl-list-panel">
          <div className="pl-list-head">
            <span>
              {section === "submissions"
                ? "Bundle intake & verification"
                : section === "reviews"
                  ? "Assigned reviews & audits"
                  : "Landing queue & batches"}
            </span>
            <StatusLabel variant="chip" tone="muted">
              {section === "submissions"
                ? visibleSubmissions.length
                : section === "reviews"
                  ? reviews.length
                  : landing.length}
            </StatusLabel>
          </div>

          {section === "submissions" &&
            (visibleSubmissions.length === 0 ? (
              <div className="pl-empty">No submissions match the selected filters.</div>
            ) : (
              <ol className="pl-rows">
                {visibleSubmissions.slice(0, visibleLimit).map((sub) => {
                  const stages = stageStatuses(sub);
                  return (
                    <li key={sub.sha256}>
                      <button
                        type="button"
                        className="pl-row"
                        aria-pressed={activeSubmission?.sha256 === sub.sha256}
                        onClick={() => setSelectedKey(sub.sha256)}
                      >
                        <span className="mono">{sub.task}</span>
                        <span className="pl-row-main">
                          <span className="pl-row-title">
                            {sub.reviewSummary ?? `Attempt ${sub.attempt} · ${sub.head || sub.shortSha}`}
                          </span>
                          <span className="pl-row-meta">
                            <code>{sub.shortSha}</code>
                            <span>·</span>
                            <span>{sub.member}</span>
                            <span>·</span>
                            <span>risk: {sub.risk}</span>
                          </span>
                        </span>
                        <div className="pl-stages" aria-label={stages.map((state, index) => `${deliveryStages[index].label}: ${stageStateLabels[state]}`).join(", ")}>
                          {stages.map((st, idx) => (
                            <span key={idx} className={`pl-stage-dot is-${st}`} />
                          ))}
                        </div>
                        <StatusLabel variant="pill" tone={submissionStateTone(sub.state)}>
                          {sub.state.replaceAll("_", " ")}
                        </StatusLabel>
                      </button>
                    </li>
                  );
                })}
              </ol>
            ))}

          {section === "reviews" &&
            (reviews.length === 0 ? (
              <div className="pl-empty">No active or recent review assignments.</div>
            ) : (
              <ol className="pl-rows">
                {reviews.slice(0, visibleLimit).map((rev) => {
                  const key = `${rev.task}#${rev.n}`;
                  return (
                    <li key={key}>
                      <button
                        type="button"
                        className="pl-row"
                        aria-pressed={activeReview && `${activeReview.task}#${activeReview.n}` === key}
                        onClick={() => setSelectedKey(key)}
                      >
                        <span className="mono">{rev.task}</span>
                        <span className="pl-row-main">
                          <span className="pl-row-title">
                            {rev.role === "audit" ? "Audit" : "Required review"} #{rev.n} · {rev.reviewer}
                          </span>
                          <span className="pl-row-meta">
                            <code>{rev.submission}</code>
                            {rev.crossFamily && <span>· cross-family</span>}
                            {rev.due && <span>· due {rev.due}</span>}
                          </span>
                        </span>
                        <StatusLabel variant="chip" tone={rev.role === "audit" ? "warning" : "accent"}>
                          {rev.role}
                        </StatusLabel>
                        <StatusLabel
                          variant="pill"
                          tone={
                            rev.verdict === "approve" || rev.verdict === "pass"
                              ? "good"
                              : rev.state === "assigned"
                                ? "warning"
                                : "muted"
                          }
                        >
                          {rev.verdict ?? rev.state}
                        </StatusLabel>
                      </button>
                    </li>
                  );
                })}
              </ol>
            ))}

          {section === "landing" &&
            (landing.length === 0 ? (
              <div className="pl-empty">Landing queue is empty.</div>
            ) : (
              <ol className="pl-rows">
                {landing.slice(0, visibleLimit).map((item) => (
                  <li key={item.refName}>
                    <button
                      type="button"
                      className="pl-row"
                      aria-pressed={activeLanding?.refName === item.refName}
                      onClick={() => setSelectedKey(item.refName)}
                    >
                      <span className="mono">{item.taskId || item.sha}</span>
                      <span className="pl-row-main">
                        <span className="pl-row-title">{item.refName}</span>
                        <span className="pl-row-meta">
                          <code>{item.sha}</code>
                          <span>·</span>
                          <span>priority: {item.priority}</span>
                          {item.batchId && <span>· {item.batchId}</span>}
                        </span>
                      </span>
                      <span className="mono">{item.enqueuedAt}</span>
                      <StatusLabel
                        variant="pill"
                        tone={item.state === "landed" ? "good" : item.failStep ? "danger" : "warning"}
                      >
                        {item.state.replaceAll("_", " ")}
                      </StatusLabel>
                    </button>
                  </li>
                ))}
              </ol>
            ))}
          {entryCount > visibleLimit && <div className="pl-list-more">
            <Button size="sm" onClick={() => setVisibleLimit(visibleLimit + pageSize)}>Show more</Button>
          </div>}
        </div>

        {activeSubmission && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Submission detail"
            eyebrow={
              <>
                <ShieldCheck size={15} />
                <span className="mono">{activeSubmission.task}</span>
              </>
            }
            title={`Submission ${activeSubmission.shortSha}`}
          >
            {activeSubmission.reviewSummary && <p className="pl-detail-copy">{activeSubmission.reviewSummary}</p>}
            <SubmissionTrace submission={activeSubmission} />
            <DefinitionList
              items={[
                {
                  label: "State",
                  value: (
                    <StatusLabel variant="pill" tone={submissionStateTone(activeSubmission.state)}>
                      {activeSubmission.state.replaceAll("_", " ")}
                    </StatusLabel>
                  ),
                },
                { label: "Member", value: activeSubmission.member },
                { label: "Risk / Tier", value: `${activeSubmission.risk} · Tier ${activeSubmission.tier}` },
                {
                  label: "Base / Head",
                  value: <CodeChip small>{`${activeSubmission.base}..${activeSubmission.head}`}</CodeChip>,
                },
                {
                  label: "Static gate",
                  value:
                    activeSubmission.gatePassed === undefined
                      ? "Tracked on hub"
                      : activeSubmission.gatePassed
                        ? "Passed (0 findings)"
                        : `Failed (${activeSubmission.gateFindings ?? 1} findings)`,
                },
                {
                  label: "Clean verify",
                  value: activeSubmission.verifyRunId
                    ? `run-${activeSubmission.verifyRunId} · ${activeSubmission.verifyConclusion ?? activeSubmission.verifyState}`
                    : "Pending",
                },
                { label: "Updated", value: <span className="mono">{activeSubmission.updatedAt}</span> },
              ]}
            />
            <div className="ui-inspector-actions ui-inspector-actions-docked">
              <Button variant="link" onClick={() => onOpenTask(activeSubmission.task)}>
                Open task <ArrowRight size={14} />
              </Button>
              <Button variant="link" onClick={() => onMessages(`submission/${activeSubmission.shortSha}*`)}>
                Open pipeline log <ArrowRight size={14} />
              </Button>
            </div>
          </InspectorPanel>
        )}

        {activeReview && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Review detail"
            eyebrow={
              <>
                <Scales size={15} />
                <span className="mono">{activeReview.task}</span>
              </>
            }
            title={`${activeReview.role === "audit" ? "Audit" : "Review"} #${activeReview.n}`}
          >
            <p className="pl-detail-copy">
              {activeReview.findings ?? "Awaiting signed verdict from assigned reviewer."}
            </p>
            <DefinitionList
              items={[
                { label: "Reviewer", value: activeReview.reviewer },
                { label: "Role", value: activeReview.role },
                { label: "Submission", value: <CodeChip small>{activeReview.submission}</CodeChip> },
                { label: "State", value: activeReview.verdict ?? activeReview.state },
                { label: "Cross-family", value: activeReview.crossFamily ? "Enforced" : "Standard" },
                ...(activeReview.due ? [{ label: "Due", value: <span className="mono">{activeReview.due}</span> }] : []),
              ]}
            />
            <div className="ui-inspector-actions ui-inspector-actions-docked">
              <Button variant="link" onClick={() => onMessages(`review/${activeReview.task}/*`)}>
                Open review events <ArrowRight size={14} />
              </Button>
            </div>
          </InspectorPanel>
        )}

        {activeLanding && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Landing detail"
            eyebrow={
              <>
                <GitCommit size={15} />
                <span className="mono">{activeLanding.taskId || activeLanding.sha}</span>
              </>
            }
            title={activeLanding.refName}
          >
            <DefinitionList
              items={[
                { label: "State", value: activeLanding.state.replaceAll("_", " ") },
                { label: "Commit", value: <CodeChip small>{activeLanding.sha}</CodeChip> },
                { label: "Priority", value: activeLanding.priority },
                { label: "Enqueued", value: <span className="mono">{activeLanding.enqueuedAt}</span> },
                ...(activeLanding.approvedBy ? [{ label: "Approved by", value: activeLanding.approvedBy }] : []),
                ...(activeLanding.failStep ? [{ label: "Failed step", value: activeLanding.failStep }] : []),
                ...(activeLanding.batchId
                  ? [
                      {
                        label: "Batch",
                        value:
                          batches.find((b) => b.batchId === activeLanding.batchId)?.note ??
                          activeLanding.batchId,
                      },
                    ]
                  : []),
              ]}
            />
            <div className="ui-inspector-actions ui-inspector-actions-docked">
              {activeLanding.taskId && (
                <Button variant="link" onClick={() => onOpenTask(activeLanding.taskId)}>
                  Open task <ArrowRight size={14} />
                </Button>
              )}
              <Button variant="link" onClick={() => onMessages("land/*")}>
                Open landing events <ArrowRight size={14} />
              </Button>
            </div>
          </InspectorPanel>
        )}
      </div>
    </section>
  );
}
