import { useState } from "react";
import { ArrowRight, ChartLineDown, Desktop, Info, TreeStructure } from "@phosphor-icons/react";
import type { FeatureItem, MatrixCell, RegressionCheck, RunOrderItem } from "../api/types";
import {
  Button,
  CodeChip,
  DefinitionList,
  FilterBar,
  FilterField,
  InspectorPanel,
  SegmentedControl,
  StatusLabel,
  featureStateTone,
} from "../components";
import { FeatureCards, MachineMatrix, matrixCellKey } from "./FeatureAtlas";
import "./pipeline.css";
import "./testing.css";

type TestingSection = "features" | "matrix" | "regressions";

export default function TestingView({
  features,
  matrix,
  matrixBlocksNote,
  runOrders,
  regressions,
  onOpenTask,
  onMessages,
}: {
  features: FeatureItem[];
  matrix: MatrixCell[];
  matrixBlocksNote?: { kept: boolean; text: string };
  runOrders: RunOrderItem[];
  regressions: RegressionCheck[];
  onOpenTask: (taskId: string) => void;
  onMessages: (topic: string) => void;
}) {
  const [section, setSection] = useState<TestingSection>("features");
  const [selectedId, setSelectedId] = useState<string | null>(features[0]?.id ?? null);

  const [areaFilter, setAreaFilter] = useState("");
  const [featureSearch, setFeatureSearch] = useState("");
  const [proofFilter, setProofFilter] = useState<{ profile: string; scenario: string } | null>(null);
  const areas = new Map(features.map(({ area, areaTitle }) => [area, areaTitle]));
  const query = featureSearch.trim().toLowerCase();
  const visibleFeatures = features.filter((feature) =>
    (!areaFilter || feature.area === areaFilter)
    && (!query || [feature.id, feature.title, feature.areaTitle].some((value) => value.toLowerCase().includes(query))),
  );
  const visibleMatrix = matrix.filter((cell) => !proofFilter
    || (cell.profile === proofFilter.profile && (!cell.scenario || cell.scenario === proofFilter.scenario)));

  function selectFeature(id: string) {
    setAreaFilter("");
    setFeatureSearch("");
    setSelectedId(id);
  }

  const activeFeature = section === "features"
    ? visibleFeatures.find((f) => f.id === selectedId) ?? visibleFeatures[0]
    : undefined;
  const activeCell = section === "matrix"
    ? visibleMatrix.find((cell) => matrixCellKey(cell) === selectedId) ?? visibleMatrix[0]
    : undefined;
  const activeReg =
    section === "regressions"
      ? regressions.find((r) => `${r.sha}/${r.machineClass}/${r.metric}` === selectedId) ?? regressions[0]
      : undefined;

  const hasDetail = Boolean(activeFeature || activeCell || activeReg);
  const dependents = activeFeature ? features.filter((feature) => feature.depends.includes(activeFeature.id)) : [];

  return (
    <section className="ts-view" aria-labelledby="ts-title">
      <div className="ts-header">
        <div>
          <h1 id="ts-title">Feature map & test network</h1>
          <p>See what works, what depends on it, and which machines proved it.</p>
        </div>
        <SegmentedControl
          ariaLabel="Testing section"
          value={section}
          onChange={(next) => {
            setSection(next);
            setSelectedId(null);
            setProofFilter(null);
          }}
          options={[
            { value: "features", label: `Feature map (${features.length})` },
            { value: "matrix", label: `Test matrix (${matrix.length})` },
            { value: "regressions", label: `Regressions (${regressions.length})` },
          ]}
        />
      </div>

      {section === "features" && features.length > 0 && (
        <FilterBar ariaLabel="Feature map filters">
          <FilterField label="Area"><select value={areaFilter} onChange={(event) => setAreaFilter(event.target.value)}>
            <option value="">All areas</option>
            {[...areas].map(([area, title]) => <option key={area} value={area}>{title}</option>)}
          </select></FilterField>
          <FilterField label="Find feature"><input value={featureSearch} onChange={(event) => setFeatureSearch(event.target.value)} placeholder="Title or feature ID" /></FilterField>
          {(areaFilter || featureSearch) && <Button size="sm" variant="ghost" onClick={() => { setAreaFilter(""); setFeatureSearch(""); }}>Clear filters</Button>}
        </FilterBar>
      )}

      {section === "matrix" && proofFilter && (
        <FilterBar ariaLabel="Proof filter">
          <CodeChip small>{proofFilter.profile} / {proofFilter.scenario}</CodeChip>
          <Button size="sm" variant="ghost" onClick={() => setProofFilter(null)}>Show all reports</Button>
        </FilterBar>
      )}

      {section === "matrix" && matrixBlocksNote && !matrixBlocksNote.kept && (
        <div className="ts-notice">
          <Info size={16} />
          <span>{matrixBlocksNote.text}</span>
        </div>
      )}

      <div className="ts-content" data-detail={hasDetail}>
        <div>
          <div className="ts-list-panel">
            <div className="ts-list-head">
              <span>
                {section === "features"
                  ? "Capabilities by area"
                  : section === "matrix"
                    ? "Evidence by machine"
                    : "Metric baseline & MAD checks"}
              </span>
              <StatusLabel variant="chip" tone="muted">
                {section === "features"
                  ? visibleFeatures.length
                  : section === "matrix"
                    ? visibleMatrix.length
                    : regressions.length}
              </StatusLabel>
            </div>

            {section === "features" && (features.length
              ? <FeatureCards features={visibleFeatures} active={activeFeature} onSelect={setSelectedId} />
              : <div className="ts-empty">No feature map is available from this hub.</div>)}

            {section === "matrix" && <MachineMatrix matrix={visibleMatrix} active={activeCell} onSelect={setSelectedId} />}

            {section === "regressions" &&
              (regressions.length === 0 ? (
                <div className="ts-empty">No metric checks recorded yet.</div>
              ) : (
                <ol className="ts-rows">
                  {regressions.map((reg) => {
                    const key = `${reg.sha}/${reg.machineClass}/${reg.metric}`;
                    return (
                      <li key={key}>
                        <button
                          type="button"
                          className="ts-row"
                          aria-pressed={
                            activeReg &&
                            `${activeReg.sha}/${activeReg.machineClass}/${activeReg.metric}` === key
                          }
                          onClick={() => setSelectedId(key)}
                        >
                          <span className="mono">{reg.sha}</span>
                          <span className="ts-row-main">
                            <span className="ts-row-title">
                              {reg.metric} · {reg.machineClass}
                            </span>
                            <span className="ts-row-meta">
                              <span>scenario: {reg.scenario}</span>
                              <span>·</span>
                              <span>candidate: {reg.candidateMedian}</span>
                              {reg.baselineMedian !== undefined && <span>· base: {reg.baselineMedian}</span>}
                            </span>
                          </span>
                          <span className="mono">{reg.madGuard !== undefined ? `MAD ±${reg.madGuard}` : "—"}</span>
                          <StatusLabel
                            variant="pill"
                            tone={
                              reg.status === "regression"
                                ? "danger"
                                : reg.status === "good"
                                  ? "good"
                                  : "warning"
                            }
                          >
                            {reg.status.replaceAll("_", " ")}
                          </StatusLabel>
                        </button>
                      </li>
                    );
                  })}
                </ol>
              ))}
          </div>

          {section === "matrix" && runOrders.length > 0 && (
            <div className="ts-order-card">
              <h2>Open run orders</h2>
              {runOrders.map((order) => (
                <div key={order.id} className="ts-order-row">
                  <span className="mono">
                    {order.id} · {order.profile} {order.scenario}
                  </span>
                  <span className="mono">
                    {order.needsOs ?? "any"} {order.needsGpu ? `/ ${order.needsGpu}` : ""} · {order.reports}/
                    {order.wanted} reports
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>

        {activeFeature && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Feature detail"
            eyebrow={
              <>
                <TreeStructure size={15} />
                <span className="mono">{activeFeature.id}</span>
              </>
            }
            title={activeFeature.title}
          >
            {activeFeature.how && <p className="ts-detail-copy">{activeFeature.how}</p>}
            <DefinitionList
              items={[
                { label: "Area", value: activeFeature.areaTitle },
                { label: "Open orders", value: `${activeFeature.openTasks} tasks · ${activeFeature.openRuns} runs` },

                ...(activeFeature.paths.length > 0
                  ? [
                      {
                        label: "Gate globs",
                        value: (
                          <div className="ts-state-pills">
                            {activeFeature.paths.map((p) => (
                              <CodeChip key={p} small>
                                {p}
                              </CodeChip>
                            ))}
                          </div>
                        ),
                      },
                    ]
                  : []),
              ]}
            />
            {(activeFeature.depends.length > 0 || dependents.length > 0) && (
              <section className="feature-detail-section" aria-label="Feature dependencies">
                <h3>Dependencies</h3>
                <ul className="feature-evidence">
                  {activeFeature.depends.map((id) => (
                    <li key={id}><span>Requires</span>
                      {features.some((feature) => feature.id === id)
                        ? <Button variant="link" onClick={() => selectFeature(id)}><code>{id}</code> <ArrowRight size={13} /></Button>
                        : <code>{id}</code>}
                    </li>
                  ))}
                  {dependents.map(({ id }) => (
                    <li key={id}><span>Required by</span><Button variant="link" onClick={() => selectFeature(id)}><code>{id}</code> <ArrowRight size={13} /></Button></li>
                  ))}
                </ul>
              </section>
            )}
            <section className="feature-detail-section" aria-label="Test evidence">
              <h3>Machine evidence</h3>
              {Object.keys(activeFeature.states).length > 0
                ? <ul className="feature-evidence">{Object.entries(activeFeature.states).map(([machine, state]) => (
                    <li key={machine}><code>{machine}</code><StatusLabel variant="dot" tone={featureStateTone(state)}>{state}</StatusLabel></li>
                  ))}</ul>
                : <p>No machine evidence reported.</p>}
            </section>
            {Boolean(activeFeature.proofs?.length) && (
              <section className="feature-detail-section" aria-label="Proof milestones">
                <h3>Proof milestones</h3>
                <ul className="feature-proofs">
                  {activeFeature.proofs?.map((proof) => (
                    <li key={`${proof.profile}/${proof.scenario}/${proof.milestone}`}>
                      <strong>{proof.milestone.replaceAll("_", " ")}</strong>
                      <Button variant="link" onClick={() => { setProofFilter(proof); setSection("matrix"); setSelectedId(null); }}>
                        <code>{proof.profile} / {proof.scenario}</code> <ArrowRight size={13} />
                      </Button>
                    </li>
                  ))}
                </ul>
              </section>
            )}
          </InspectorPanel>
        )}

        {activeCell && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Matrix cell detail"
            eyebrow={
              <>
                <Desktop size={15} />
                <span className="mono">{activeCell.profile}</span>
              </>
            }
            title={activeCell.machineKey}
          >
            <DefinitionList
              items={[
                {
                  label: "Outcome",
                  value: (
                    <StatusLabel variant="pill" tone={featureStateTone(activeCell.outcome)}>
                      {activeCell.outcome}
                    </StatusLabel>
                  ),
                },
                { label: "Scenario", value: activeCell.scenario ?? "All scenarios in profile" },
                { label: "Reports", value: activeCell.reports },
                { label: "Quorum threshold", value: activeCell.quorum },
                { label: "Build", value: <CodeChip small>{activeCell.build}</CodeChip> },
                ...(activeCell.channel ? [{ label: "Channel", value: activeCell.channel }] : []),
              ]}
            />
            <div className="ui-inspector-actions ui-inspector-actions-docked">
              <Button variant="link" onClick={() => onMessages("report/*")}>
                Open test reports <ArrowRight size={14} />
              </Button>
            </div>
          </InspectorPanel>
        )}

        {activeReg && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Regression detail"
            eyebrow={
              <>
                <ChartLineDown size={15} />
                <span className="mono">{activeReg.sha}</span>
              </>
            }
            title={`${activeReg.metric} (${activeReg.scenario})`}
          >
            <DefinitionList
              items={[
                {
                  label: "Status",
                  value: (
                    <StatusLabel
                      variant="pill"
                      tone={activeReg.status === "regression" ? "danger" : activeReg.status === "good" ? "good" : "warning"}
                    >
                      {activeReg.status.replaceAll("_", " ")}
                    </StatusLabel>
                  ),
                },
                { label: "Machine class", value: activeReg.machineClass },
                { label: "Candidate median", value: String(activeReg.candidateMedian) },
                { label: "Baseline median", value: activeReg.baselineMedian !== undefined ? String(activeReg.baselineMedian) : "—" },
                { label: "MAD guard", value: activeReg.madGuard !== undefined ? `±${activeReg.madGuard}` : "—" },
              ]}
            />
            {activeReg.linkedTask && (
              <div className="ui-inspector-actions ui-inspector-actions-docked">
                <Button variant="link" onClick={() => onOpenTask(activeReg.linkedTask!)}>
                  Open linked task {activeReg.linkedTask} <ArrowRight size={14} />
                </Button>
              </div>
            )}
          </InspectorPanel>
        )}
      </div>
    </section>
  );
}
