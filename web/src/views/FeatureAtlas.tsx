import { ArrowRight, CaretRight, TreeStructure } from "@phosphor-icons/react";
import type { FeatureItem, MatrixCell } from "../api/types";
import { StatusLabel, featureStateTone } from "../components";

export function FeatureCards({ features, active, onSelect }: {
  features: FeatureItem[];
  active?: FeatureItem;
  onSelect: (id: string) => void;
}) {
  const areas = new Map<string, FeatureItem[]>();
  for (const feature of features) {
    const group = areas.get(feature.area) ?? [];
    group.push(feature);
    areas.set(feature.area, group);
  }
  if (!areas.size) return <div className="ts-empty">No features match this view.</div>;

  return (
    <div className="feature-atlas">
      {[...areas].map(([area, items]) => (
        <section className="feature-area" key={area} aria-label={items[0].areaTitle}>
          <h3>{items[0].areaTitle} <span className="mono">{items.length}</span></h3>
          <ul className="feature-grid">
            {items.map((feature) => {
              const machineStates = Object.values(feature.states);
              const passed = machineStates.filter((state) => state === "works").length;
              const failed = machineStates.filter((state) => ["fails", "broken"].includes(state)).length;
              const other = machineStates.length - passed - failed;
              const dependency = active?.depends.includes(feature.id);
              const dependent = active && feature.depends.includes(active.id);
              return (
                <li key={feature.id}>
                  <button type="button" className="feature-card" aria-pressed={active?.id === feature.id} onClick={() => onSelect(feature.id)}>
                    <span className="feature-card-id"><code>{feature.id}</code><CaretRight size={14} aria-hidden="true" /></span>
                    <strong>{feature.title}</strong>
                    <span className="feature-coverage">
                      {passed > 0 && <StatusLabel variant="dot" tone="good">{passed} confirmed</StatusLabel>}
                      {failed > 0 && <StatusLabel variant="dot" tone="danger">{failed} failed</StatusLabel>}
                      {other > 0 && <StatusLabel variant="dot" tone="warning">{other} unresolved</StatusLabel>}
                      {!machineStates.length && <span>No machine evidence</span>}
                    </span>
                    <span className="feature-card-meta">
                      {dependency || dependent ? <span className="feature-related"><TreeStructure size={14} /> {dependency ? "Dependency" : "Depends on selection"}</span>
                        : feature.depends.length ? <span>{feature.depends.length} {feature.depends.length === 1 ? "dependency" : "dependencies"}</span> : null}
                      {feature.openOrders > 0 && <span>{feature.openOrders} open</span>}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      ))}
    </div>
  );
}

export function matrixCellKey(cell: MatrixCell) {
  return JSON.stringify([cell.profile, cell.scenario, cell.build, cell.channel, cell.machineKey]);
}

export function MachineMatrix({ matrix, active, onSelect }: {
  matrix: MatrixCell[];
  active?: MatrixCell;
  onSelect: (key: string) => void;
}) {
  if (!matrix.length) return <div className="ts-empty">No matching matrix reports.</div>;
  const machines = [...new Set(matrix.map(({ machineKey }) => machineKey))].sort();
  const groups = new Map<string, { sample: MatrixCell; cells: Map<string, MatrixCell> }>();
  for (const cell of matrix) {
    const rowKey = JSON.stringify([cell.profile, cell.scenario, cell.build, cell.channel]);
    const group = groups.get(rowKey) ?? { sample: cell, cells: new Map() };
    group.cells.set(cell.machineKey, cell);
    groups.set(rowKey, group);
  }

  return (
    <div className="test-matrix-scroll">
      <table className="test-matrix">
        <caption className="sr-only">Test outcomes by scenario, build and machine</caption>
        <thead><tr><th scope="col">Profile / scenario</th>{machines.map((machine) => <th key={machine} scope="col"><code>{machine}</code></th>)}</tr></thead>
        <tbody>
          {[...groups].map(([key, { sample, cells }]) => (
            <tr key={key}>
              <th scope="row">
                <strong>{sample.profile}{sample.scenario ? ` / ${sample.scenario}` : ""}</strong>
                <small><code>{sample.build}</code>{sample.channel ? ` · ${sample.channel}` : ""}{!sample.scenario ? " · All scenarios" : ""}</small>
              </th>
              {machines.map((machine) => {
                const cell = cells.get(machine);
                if (!cell) return <td key={machine}><span className="matrix-unreported">—<span className="sr-only"> No reports</span></span></td>;
                return (
                  <td key={machine}>
                    <button type="button" className="test-matrix-cell"
                      aria-pressed={Boolean(active && matrixCellKey(active) === matrixCellKey(cell))}
                      aria-label={`${cell.profile}${cell.scenario ? ` / ${cell.scenario}` : ""}, ${machine}, ${cell.outcome}, ${cell.reports} reports`}
                      onClick={() => onSelect(matrixCellKey(cell))}>
                      <StatusLabel variant="dot" tone={featureStateTone(cell.outcome)}>{cell.outcome}</StatusLabel>
                      <small>{cell.reports} {cell.reports === 1 ? "report" : "reports"} <ArrowRight size={12} aria-hidden="true" /></small>
                    </button>
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
