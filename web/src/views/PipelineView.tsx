import { useEffect, useMemo, useState } from "react";
import {
  ArrowRight,
  Check,
  CheckCircle,
  Circle,
  Clock,
  Copy,
  GitBranch,
  GitCommit,
  Scales,
  ShieldCheck,
  Stack,
  WarningCircle,
  X,
} from "@phosphor-icons/react";
import { formatBatchLabel, splitRefName } from "../api/format";
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

function initialSectionFor(
  submissions: SubmissionItem[],
  reviews: ReviewItem[],
  landing: LandingItem[],
  initialTaskFilter: string | null,
): PipelineSection {
  if (initialTaskFilter || submissions.length > 0) return "submissions";
  if (reviews.length > 0) return "reviews";
  if (landing.length > 0) return "landing";
  return "submissions";
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
  const [section, setSection] = useState<PipelineSection>(() =>
    initialSectionFor(submissions, reviews, landing, initialTaskFilter),
  );
  const pageSize = 24;
  const [visibleLimit, setVisibleLimit] = useState(pageSize);
  const [stateFilter, setStateFilter] = useState("");
  const [stageFilter, setStageFilter] = useState<number | null>(null);
  const [landingQuery, setLandingQuery] = useState("");
  const [landingStateFilter, setLandingStateFilter] = useState("");
  const [copiedSha, setCopiedSha] = useState(false);
  const [selectedKey, setSelectedKey] = useState<string | null>(
    () =>
      submissions.find((s) => s.task === initialTaskFilter)?.sha256 ??
      submissions[0]?.sha256 ??
      landing[0]?.refName ??
      null,
  );

  useEffect(() => {
    if (!initialTaskFilter) return;
    setSection("submissions");
    setStageFilter(null);
    setStateFilter("");
    setSelectedKey(null);
  }, [initialTaskFilter]);

  useEffect(() => {
    setCopiedSha(false);
  }, [selectedKey]);

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

  const visibleLanding = useMemo(() => {
    const q = landingQuery.trim().toLowerCase();
    return landing.filter(
      (item) =>
        (!landingStateFilter || item.state === landingStateFilter) &&
        (!q ||
          [item.refName, item.sha, item.taskId, item.batchId ?? ""].some((v) =>
            v.toLowerCase().includes(q),
          )),
    );
  }, [landing, landingQuery, landingStateFilter]);

  const landingStats = useMemo(() => {
    const landed = landing.filter((l) => l.state === "landed").length;
    const failed = landing.filter((l) => Boolean(l.failStep) || l.state.includes("fail")).length;
    const queued = landing.length - landed - failed;
    const uniqueBatches = new Set(landing.map((l) => l.batchId).filter(Boolean)).size;
    return { landed, queued, failed, uniqueBatches };
  }, [landing]);

  const landingStates = useMemo(
    () => [...new Set(landing.map((l) => l.state))].sort(),
    [landing],
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
      ? visibleLanding.find((l) => `${l.refName}@${l.sha}` === selectedKey || l.refName === selectedKey) ??
        visibleLanding[0]
      : undefined;

  const hasDetail = Boolean(activeSubmission || activeReview || activeLanding);
  const entryCount =
    section === "submissions"
      ? visibleSubmissions.length
      : section === "reviews"
        ? reviews.length
        : visibleLanding.length;
  const submissionStates = useMemo(
    () => [...new Set(submissions.map((s) => s.state))].sort(),
    [submissions],
  );

  async function copySha(sha: string) {
    try {
      await navigator.clipboard.writeText(sha);
      setCopiedSha(true);
      window.setTimeout(() => setCopiedSha(false), 1800);
    } catch {
      // Ignore clipboard failures.
    }
  }

  return (
    <section className="pl-view" aria-labelledby="pl-title">
      <div className="pl-header">
        <div>
          <h1 id="pl-title">Delivery pipeline</h1>
          <p>Follow submissions, independent reviews, and landed commits from intake to main.</p>
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
              <button
                key={label}
                type="button"
                aria-pressed={stageFilter === index}
                onClick={() => {
                  setStageFilter(stageFilter === index ? null : index);
                  setStateFilter("");
                  setVisibleLimit(pageSize);
                }}
              >
                <span className="pl-stage-icon">
                  <Icon size={17} aria-hidden="true" />
                </span>
                <span>
                  <strong>{label}</strong>
                  <small>{description}</small>
                </span>
                <span className="pl-stage-total mono">{count}</span>
              </button>
            );
          })}
        </div>
      )}

      {section === "landing" && landing.length > 0 && (
        <div className="pl-stage-strip" role="group" aria-label="Landing summary">
          <button
            type="button"
            aria-pressed={landingStateFilter === "landed"}
            onClick={() => setLandingStateFilter(landingStateFilter === "landed" ? "" : "landed")}
          >
            <span className="pl-stage-icon is-good">
              <CheckCircle size={17} aria-hidden="true" />
            </span>
            <span>
              <strong>Landed</strong>
              <small>Merged to main</small>
            </span>
            <span className="pl-stage-total mono">{landingStats.landed}</span>
          </button>
          <button
            type="button"
            aria-pressed={landingStateFilter === "queued"}
            onClick={() => setLandingStateFilter(landingStateFilter === "queued" ? "" : "queued")}
          >
            <span className="pl-stage-icon">
              <Clock size={17} aria-hidden="true" />
            </span>
            <span>
              <strong>In flight</strong>
              <small>Queued or verifying</small>
            </span>
            <span className="pl-stage-total mono">{landingStats.queued}</span>
          </button>
          <button
            type="button"
            aria-pressed={landingStateFilter === "failed"}
            onClick={() => setLandingStateFilter(landingStateFilter === "failed" ? "" : "failed")}
          >
            <span className="pl-stage-icon is-danger">
              <WarningCircle size={17} aria-hidden="true" />
            </span>
            <span>
              <strong>Failed</strong>
              <small>Needs attention</small>
            </span>
            <span className="pl-stage-total mono">{landingStats.failed}</span>
          </button>
          <div className="pl-stage-stat">
            <span className="pl-stage-icon">
              <Stack size={17} aria-hidden="true" />
            </span>
            <span>
              <strong>Batches</strong>
              <small>Verification runs</small>
            </span>
            <span className="pl-stage-total mono">{landingStats.uniqueBatches || batches.length}</span>
          </div>
        </div>
      )}

      {section === "submissions" && (
        <FilterBar ariaLabel="Submission filters">
          <FilterField label="State">
            <select
              value={stateFilter}
              onChange={(e) => {
                setStateFilter(e.target.value);
                setStageFilter(null);
                setVisibleLimit(pageSize);
              }}
            >
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

      {section === "landing" && landing.length > 0 && (
        <FilterBar ariaLabel="Landing filters">
          <FilterField label="Search">
            <input
              value={landingQuery}
              onChange={(e) => {
                setLandingQuery(e.target.value);
                setVisibleLimit(pageSize);
              }}
              placeholder="Branch, commit SHA, or batch"
            />
          </FilterField>
          {landingStates.length > 1 && (
            <FilterField label="State">
              <select
                value={landingStateFilter}
                onChange={(e) => {
                  setLandingStateFilter(e.target.value);
                  setVisibleLimit(pageSize);
                }}
              >
                <option value="">All states</option>
                {landingStates.map((st) => (
                  <option key={st} value={st}>
                    {st.replaceAll("_", " ")}
                  </option>
                ))}
              </select>
            </FilterField>
          )}
          {(landingQuery || landingStateFilter) && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setLandingQuery("");
                setLandingStateFilter("");
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
              {entryCount}
            </StatusLabel>
          </div>

          {section === "submissions" &&
            (visibleSubmissions.length === 0 ? (
              <div className="pl-empty">
                <ShieldCheck size={24} />
                <strong>No active submissions</strong>
                <p>
                  {landing.length > 0
                    ? `No bundles are waiting in intake right now. ${landing.length} changes are recorded in Landing.`
                    : "Bundles uploaded by contributors appear here for gate, verify, and review checks."}
                </p>
                {landing.length > 0 && (
                  <Button variant="secondary" size="sm" onClick={() => setSection("landing")}>
                    View landing history ({landing.length}) <ArrowRight size={14} />
                  </Button>
                )}
              </div>
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
                        <div
                          className="pl-stages"
                          aria-label={stages
                            .map((state, index) => `${deliveryStages[index].label}: ${stageStateLabels[state]}`)
                            .join(", ")}
                        >
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
              <div className="pl-empty">
                <Scales size={24} />
                <strong>No active review assignments</strong>
                <p>Independent reviewer and audit verdicts appear here once submissions enter review.</p>
              </div>
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
            (visibleLanding.length === 0 ? (
              <div className="pl-empty">
                <GitCommit size={24} />
                <strong>{landing.length === 0 ? "Landing queue is empty" : "No matching commits"}</strong>
                <p>
                  {landing.length === 0
                    ? "Verified branches queued for batch landing appear here."
                    : "Try clearing the search query or state filter."}
                </p>
              </div>
            ) : (
              <ol className="pl-rows">
                {visibleLanding.slice(0, visibleLimit).map((item) => {
                  const rowKey = `${item.refName}@${item.sha}`;
                  const refParts = splitRefName(item.refName);
                  const batchInfo = item.batchId ? formatBatchLabel(item.batchId) : null;
                  const metaItems: string[] = [];
                  if (item.taskId && item.sha) metaItems.push(item.sha);
                  if (item.priority) metaItems.push(`priority: ${item.priority}`);
                  if (batchInfo) metaItems.push(batchInfo.label);
                  return (
                    <li key={rowKey}>
                      <button
                        type="button"
                        className="pl-row"
                        aria-pressed={activeLanding?.refName === item.refName && activeLanding?.sha === item.sha}
                        onClick={() => setSelectedKey(rowKey)}
                      >
                        <span className="mono pl-row-id">{item.taskId || item.sha}</span>
                        <span className="pl-row-main">
                          <span className="pl-row-title" title={item.refName}>
                            {refParts.prefix && <span className="pl-ref-prefix">{refParts.prefix}</span>}
                            {refParts.name}
                          </span>
                          {metaItems.length > 0 && (
                            <span className="pl-row-meta">
                              {metaItems.map((piece, idx) => (
                                <span key={idx}>
                                  {idx > 0 ? "· " : ""}
                                  {piece}
                                </span>
                              ))}
                            </span>
                          )}
                        </span>
                        <span className="mono pl-row-time">{item.enqueuedAt}</span>
                        <StatusLabel
                          variant="pill"
                          tone={item.state === "landed" ? "good" : item.failStep ? "danger" : "warning"}
                        >
                          {item.state.replaceAll("_", " ")}
                        </StatusLabel>
                      </button>
                    </li>
                  );
                })}
              </ol>
            ))}

          {entryCount > visibleLimit && (
            <div className="pl-list-more">
              <Button size="sm" onClick={() => setVisibleLimit(visibleLimit + pageSize)}>
                Show more ({entryCount - visibleLimit} remaining)
              </Button>
            </div>
          )}
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

        {activeLanding && (() => {
          const batch = activeLanding.batchId
            ? batches.find((b) => b.batchId === activeLanding.batchId)
            : undefined;
          const batchInfo = activeLanding.batchId ? formatBatchLabel(activeLanding.batchId) : undefined;
          const refParts = splitRefName(activeLanding.refName);
          return (
            <InspectorPanel
              mode="docked"
              ariaLabel="Landing detail"
              eyebrow={
                <>
                  <GitCommit size={15} />
                  <span className="mono">{activeLanding.taskId || activeLanding.sha}</span>
                </>
              }
              title={refParts.name}
            >
              {refParts.prefix && (
                <p className="pl-detail-ref mono">{activeLanding.refName}</p>
              )}
              <DefinitionList
                items={[
                  {
                    label: "State",
                    value: (
                      <StatusLabel
                        variant="pill"
                        tone={activeLanding.state === "landed" ? "good" : activeLanding.failStep ? "danger" : "warning"}
                      >
                        {activeLanding.state.replaceAll("_", " ")}
                      </StatusLabel>
                    ),
                  },
                  {
                    label: "Commit",
                    value: (
                      <span className="pl-copy-row">
                        <CodeChip small>{activeLanding.sha}</CodeChip>
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() => copySha(activeLanding.sha)}
                          aria-label="Copy commit SHA"
                        >
                          {copiedSha ? <Check size={13} /> : <Copy size={13} />}
                        </Button>
                      </span>
                    ),
                  },
                  ...(activeLanding.priority ? [{ label: "Priority", value: activeLanding.priority }] : []),
                  { label: "Enqueued", value: <span className="mono">{activeLanding.enqueuedAt}</span> },
                  ...(activeLanding.approvedBy ? [{ label: "Approved by", value: activeLanding.approvedBy }] : []),
                  ...(activeLanding.failStep ? [{ label: "Failed step", value: activeLanding.failStep }] : []),
                  ...(activeLanding.batchId
                    ? [
                        {
                          label: "Batch",
                          value: (
                            <span title={activeLanding.batchId}>
                              {batchInfo?.label ?? activeLanding.batchId}
                              {batch?.note ? ` · ${batch.note}` : ""}
                            </span>
                          ),
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
          );
        })()}
      </div>
    </section>
  );
}
