import type { QuotaPool } from "../api/types";
import { ProviderMark } from "./ProviderMark";
import { StatusLabel, poolStateTone } from "./StatusLabel";

export function MeterBar({
  pool,
  onSelectTask,
}: {
  pool: QuotaPool;
  onSelectTask?: (taskId: string) => void;
}) {
  if (pool.redacted) {
    return (
      <article className="ui-meter">
        <div className="ui-meter-head">
          <div className="ui-meter-title">
            <ProviderMark provider={pool.provider} size={16} />
            <strong className="mono">{pool.pool}</strong>
            <span className="mono">{pool.window}</span>
          </div>
          <StatusLabel variant="pill" tone={poolStateTone(pool.state)}>
            {pool.state}
          </StatusLabel>
        </div>
        <div className="ui-meter-meta">
          <span>Public view · meter percentages redacted</span>
          <span className="mono">resets_at {pool.resetsAt > 0 ? new Date(pool.resetsAt * 1000).toLocaleTimeString("en-GB", { hour12: false }) : "—"}</span>
        </div>
      </article>
    );
  }

  const used = Math.min(100, Math.max(0, pool.usedPct));
  const committed = Math.min(100 - used, Math.max(0, pool.committedPct));
  const line = Math.min(100, Math.max(0, pool.linePct));
  const reserve = Math.min(100, Math.max(0, pool.reservePct));

  return (
    <article className={`ui-meter ${pool.stale ? "is-stale" : ""}`}>
      <div className="ui-meter-head">
        <div className="ui-meter-title">
          <ProviderMark provider={pool.provider} size={16} />
          <strong>{pool.owner}</strong>
          <span className="mono">{pool.pool}</span>
          <StatusLabel variant="chip" tone="muted">
            {pool.source} · {pool.band}
          </StatusLabel>
        </div>
        <StatusLabel variant="pill" tone={poolStateTone(pool.state)}>
          {pool.drain ? "drain" : pool.state}
        </StatusLabel>
      </div>

      <div
        className="ui-meter-track"
        role="img"
        aria-label={`${pool.pool} used ${used.toFixed(0)}%, committed ${committed.toFixed(0)}%, worker line ${line.toFixed(0)}%`}
      >
        <div className="ui-meter-used" style={{ width: `${used}%` }} />
        {committed > 0 && (
          <div className="ui-meter-committed" style={{ left: `${used}%`, width: `${committed}%` }} />
        )}
        {reserve > 0 && <div className="ui-meter-reserve" style={{ width: `${reserve}%` }} />}
        <div className="ui-meter-line" style={{ left: `${line}%` }} title={`Worker line ${line}%`} />
      </div>

      <div className="ui-meter-meta">
        <span>{pool.forecastLabel}</span>
        <span className="mono">
          {used.toFixed(0)}% used · {committed.toFixed(0)}% committed · {pool.burnPctPerHour.toFixed(1)}%/h ·{" "}
          {pool.activeLeases} {pool.activeLeases === 1 ? "lease" : "leases"} · {Math.round(pool.ageSecs / 60)}m old
        </span>
      </div>

      {pool.waitingTasks.length > 0 && (
        <div className="ui-meter-waiting">
          {pool.waitingTasks.map((waiting) => (
            <div key={waiting.id} className="ui-meter-waiting-row">
              <span>
                {onSelectTask ? (
                  <button type="button" className="ui-btn ui-btn-link mono" onClick={() => onSelectTask(waiting.id)}>
                    {waiting.id}
                  </button>
                ) : (
                  <span className="mono">{waiting.id}</span>
                )}{" "}
                · {waiting.title}
              </span>
              <StatusLabel tone="warning" variant="pill">
                {waiting.reason}
              </StatusLabel>
            </div>
          ))}
        </div>
      )}
    </article>
  );
}
