import { useEffect, useMemo, useState } from "react";
import { ArrowRight, GitCommit, Scales, ShieldCheck, X } from "@phosphor-icons/react";
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

function stageStatuses(sub: SubmissionItem): ("pass" | "active" | "fail" | "idle")[] {
  const s = sub.state;
  const gate =
    s === "gate_failed" ? "fail" : s === "received" ? "active" : "pass";
  const verify =
    s === "verify_failed"
      ? "fail"
      : s === "verifying"
        ? "active"
        : ["verified", "reviewing", "review_failed", "approved", "landed"].includes(s)
          ? "pass"
          : "idle";
  const review =
    s === "review_failed" || s === "rejected"
      ? "fail"
      : s === "reviewing"
        ? "active"
        : ["approved", "landed"].includes(s)
          ? "pass"
          : "idle";
  const land = s === "landed" ? "pass" : s === "approved" ? "active" : "idle";
  return [gate, verify, review, land];
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
  const [stateFilter, setStateFilter] = useState("");
  const [selectedKey, setSelectedKey] = useState<string | null>(
    () =>
      submissions.find((s) => s.task === initialTaskFilter)?.sha256 ??
      submissions[0]?.sha256 ??
      null,
  );

  useEffect(() => {
    if (!initialTaskFilter) return;
    setSection("submissions");
    const match = submissions.find((s) => s.task === initialTaskFilter);
    if (match) setSelectedKey(match.sha256);
  }, [initialTaskFilter, submissions]);

  const visibleSubmissions = useMemo(
    () =>
      submissions.filter(
        (item) =>
          (!stateFilter || item.state === stateFilter) &&
          (!initialTaskFilter || item.task === initialTaskFilter),
      ),
    [initialTaskFilter, stateFilter, submissions],
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
  const submissionStates = useMemo(
    () => [...new Set(submissions.map((s) => s.state))].sort(),
    [submissions],
  );

  return (
    <section className="pl-view" aria-labelledby="pl-title">
      <div className="pl-header">
        <div>
          <h1 id="pl-title">Delivery pipeline</h1>
          <p>Static gates, clean verification, assigned reviews, and landing batches.</p>
        </div>
        <SegmentedControl
          ariaLabel="Pipeline stage"
          value={section}
          onChange={(next) => {
            setSection(next);
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
        <FilterBar ariaLabel="Submission filters">
          <FilterField label="State">
            <select value={stateFilter} onChange={(e) => setStateFilter(e.target.value)}>
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
          {(stateFilter || initialTaskFilter) && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setStateFilter("");
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
                {visibleSubmissions.map((sub) => {
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
                        <div className="pl-stages" aria-label="Gate, Verify, Review, Land">
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
                {reviews.map((rev) => {
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
                {landing.map((item) => (
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
            <p className="pl-detail-copy">
              {activeSubmission.reviewSummary ?? "Bundle tracked by intake, verify, and review pipeline."}
            </p>
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
