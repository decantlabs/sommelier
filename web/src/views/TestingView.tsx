import { useMemo, useState } from "react";
import { ArrowRight, ChartLineDown, Desktop, Info, TreeStructure } from "@phosphor-icons/react";
import type { FeatureItem, MatrixCell, RegressionCheck, RunOrderItem } from "../api/types";
import {
  Button,
  CodeChip,
  DefinitionList,
  InspectorPanel,
  SegmentedControl,
  StatusLabel,
  featureStateTone,
} from "../components";
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

  const areas = useMemo(() => {
    const grouped = new Map<string, FeatureItem[]>();
    for (const f of features) {
      const list = grouped.get(f.area) ?? [];
      list.push(f);
      grouped.set(f.area, list);
    }
    return [...grouped.entries()];
  }, [features]);

  const activeFeature =
    section === "features"
      ? features.find((f) => f.id === selectedId) ?? features[0]
      : undefined;
  const activeCell =
    section === "matrix"
      ? matrix.find((c) => `${c.profile}/${c.scenario}/${c.machineKey}` === selectedId) ?? matrix[0]
      : undefined;
  const activeReg =
    section === "regressions"
      ? regressions.find((r) => `${r.sha}/${r.machineClass}/${r.metric}` === selectedId) ?? regressions[0]
      : undefined;

  const hasDetail = Boolean(activeFeature || activeCell || activeReg);

  return (
    <section className="ts-view" aria-labelledby="ts-title">
      <div className="ts-header">
        <div>
          <h1 id="ts-title">Feature map & test network</h1>
          <p>Product capability coverage, quorum matrix reports, and MAD regression guards.</p>
        </div>
        <SegmentedControl
          ariaLabel="Testing section"
          value={section}
          onChange={(next) => {
            setSection(next);
            setSelectedId(null);
          }}
          options={[
            { value: "features", label: `Features (${features.length})` },
            { value: "matrix", label: `Test matrix (${matrix.length})` },
            { value: "regressions", label: `Regressions (${regressions.length})` },
          ]}
        />
      </div>

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
                  ? "features.toml capabilities"
                  : section === "matrix"
                    ? "Hardware quorum cells"
                    : "Metric baseline & MAD checks"}
              </span>
              <StatusLabel variant="chip" tone="muted">
                {section === "features"
                  ? features.length
                  : section === "matrix"
                    ? matrix.length
                    : regressions.length}
              </StatusLabel>
            </div>

            {section === "features" &&
              (areas.length === 0 ? (
                <div className="ts-empty">No feature map configured on this hub.</div>
              ) : (
                areas.map(([area, items]) => (
                  <div key={area} className="ts-area-group">
                    <div className="ts-area-label">{items[0]?.areaTitle ?? area}</div>
                    <ol className="ts-rows">
                      {items.map((f) => (
                        <li key={f.id}>
                          <button
                            type="button"
                            className="ts-row"
                            aria-pressed={activeFeature?.id === f.id}
                            onClick={() => setSelectedId(f.id)}
                          >
                            <span className="mono">{f.id}</span>
                            <span className="ts-row-main">
                              <span className="ts-row-title">{f.title}</span>
                              <span className="ts-row-meta">
                                {f.paths[0] && <code>{f.paths[0]}</code>}
                                {f.depends.length > 0 && <span>· depends: {f.depends.join(", ")}</span>}
                              </span>
                            </span>
                            <div className="ts-state-pills">
                              {Object.entries(f.states).map(([mk, st]) => (
                                <StatusLabel key={mk} variant="pill" tone={featureStateTone(st)}>
                                  {mk}: {st}
                                </StatusLabel>
                              ))}
                            </div>
                            <StatusLabel variant="chip" tone={f.openOrders > 0 ? "accent" : "muted"}>
                              {f.openOrders} open
                            </StatusLabel>
                          </button>
                        </li>
                      ))}
                    </ol>
                  </div>
                ))
              ))}

            {section === "matrix" &&
              (matrix.length === 0 ? (
                <div className="ts-empty">No matrix reports recorded yet.</div>
              ) : (
                <ol className="ts-rows">
                  {matrix.map((cell) => {
                    const key = `${cell.profile}/${cell.scenario}/${cell.machineKey}`;
                    return (
                      <li key={key}>
                        <button
                          type="button"
                          className="ts-row"
                          aria-pressed={
                            activeCell &&
                            `${activeCell.profile}/${activeCell.scenario}/${activeCell.machineKey}` === key
                          }
                          onClick={() => setSelectedId(key)}
                        >
                          <span className="mono">{cell.profile}</span>
                          <span className="ts-row-main">
                            <span className="ts-row-title">
                              {cell.scenario} · {cell.machineKey}
                            </span>
                            <span className="ts-row-meta">
                              <code>{cell.build}</code>
                              <span>·</span>
                              <span>{cell.channel}</span>
                            </span>
                          </span>
                          <span className="mono">
                            {cell.reports}/{cell.quorum} quorum
                          </span>
                          <StatusLabel variant="pill" tone={featureStateTone(cell.outcome)}>
                            {cell.outcome}
                          </StatusLabel>
                        </button>
                      </li>
                    );
                  })}
                </ol>
              ))}

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
            <p className="ts-detail-copy">{activeFeature.how ?? "Feature capability tracked by features.toml."}</p>
            <DefinitionList
              items={[
                { label: "Area", value: activeFeature.areaTitle },
                { label: "Open orders", value: `${activeFeature.openTasks} tasks · ${activeFeature.openRuns} runs` },
                { label: "Depends on", value: activeFeature.depends.length ? activeFeature.depends.join(", ") : "None" },
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
            <div className="ts-detail-pills">
              {Object.entries(activeFeature.states).map(([mk, st]) => (
                <StatusLabel key={mk} variant="pill" tone={featureStateTone(st)}>
                  {mk}: {st}
                </StatusLabel>
              ))}
            </div>
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
            title={`${activeCell.scenario} · ${activeCell.machineKey}`}
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
                { label: "Quorum", value: `${activeCell.reports} / ${activeCell.quorum} independent owners` },
                { label: "Build", value: <CodeChip small>{activeCell.build}</CodeChip> },
                { label: "Channel", value: activeCell.channel },
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
