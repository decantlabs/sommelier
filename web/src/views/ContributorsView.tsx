import { useEffect, useState } from "react";
import {
  ArrowRight,
  BookOpen,
  CaretDown,
  CaretRight,
  Circuitry,
  Info,
  Key,
  ShieldCheck,
  Stamp,
  Users,
} from "@phosphor-icons/react";
import { access, seats, stateLayers } from "../api/demo";
import type { AgentMachine, Contributor, QuotaPool, RoleLease, WorkTask } from "../api/types";
import {
  Avatar,
  Button,
  DefinitionList,
  InspectorPanel,
  InspectorSection,
  MeterBar,
  SegmentedControl,
  StatusLabel,
  agentStateTone,
} from "../components";
import AgentNetwork from "./AgentNetwork";
import "./contributors.css";

type SubView = "agents" | "roster" | "pools" | "roles";

function RoleGuide() {
  const [section, setSection] = useState<"Access" | "Seats" | "States">("Access");

  return (
    <details className="ct-guide">
      <summary>
        <BookOpen size={16} />
        <span>Factory reference</span>
        <CaretDown className="ct-guide-caret" size={15} />
      </summary>
      <div className="ct-guide-body">
        <div className="ct-guide-nav" role="group" aria-label="Reference sections">
          {(["Access", "Seats", "States"] as const).map((item) => (
            <button
              type="button"
              key={item}
              aria-pressed={section === item}
              onClick={() => setSection(item)}
            >
              {item}
            </button>
          ))}
        </div>
        {section === "States" ? (
          <dl className="ct-state-grid">
            {stateLayers.map(([name, values]) => (
              <div key={String(name)}>
                <dt>{name}</dt>
                <dd>
                  {Array.isArray(values) && values.map((value) => <span key={value}>{value}</span>)}
                </dd>
              </div>
            ))}
          </dl>
        ) : (
          <dl className="ct-guide-definitions">
            {(section === "Access" ? access : seats).map(([title, definition]) => (
              <div key={title}>
                <dt>{title}</dt>
                <dd>{definition}</dd>
              </div>
            ))}
          </dl>
        )}
      </div>
    </details>
  );
}

export default function ContributorsView({
  contributors,
  agentMachines,
  tasks,
  currentUser,
  canRunLocally,
  pools,
  roleLeases,
  onOpenTask,
}: {
  contributors: Contributor[];
  agentMachines: AgentMachine[];
  tasks: WorkTask[];
  currentUser: string;
  canRunLocally: boolean;
  pools: QuotaPool[];
  roleLeases: RoleLease[];
  onOpenTask: (taskId: string) => void;
}) {
  const [subView, setSubView] = useState<SubView>("agents");
  const [focusNetworkOwner, setFocusNetworkOwner] = useState(false);
  const [selectedName, setSelectedName] = useState(() => contributors.find(({ name }) => name === currentUser)?.name ?? contributors[0]?.name ?? "");

  useEffect(() => {
    if (selectedName && contributors.some((c) => c.name === selectedName)) return;
    setSelectedName(contributors[0]?.name ?? "");
  }, [contributors, selectedName]);

  const selected = contributors.find(({ name }) => name === selectedName) ?? contributors[0];
  const selectedMachines = agentMachines.filter(({ owner }) => owner === selected?.name);
  const selectedAgents = selectedMachines.flatMap(({ agents }) => agents);

  return (
    <section className={`ct-view ${subView === "agents" ? "is-network" : ""}`} aria-labelledby="ct-title">
      <header className="ct-view-header">
        <div>
          <h1 id="ct-title">Contributors</h1>
          <p>People, agents, and capacity for this project.</p>
        </div>
        <div className="ct-header-actions">
          <SegmentedControl
            ariaLabel="Contributors section"
            size="sm"
            value={subView}
            onChange={(next) => { setFocusNetworkOwner(false); setSubView(next); }}
            options={[
              { value: "agents", label: "Network" },
              { value: "roster", label: "Roster" },
              { value: "pools", label: `Quota pools (${pools.length})` },
              { value: "roles", label: `Role leases (${roleLeases.length})` },
            ]}
          />
          <span className="ct-count">
            <Users size={15} /> {contributors.length} contributors
          </span>
        </div>
      </header>

      {subView === "agents" && (
        <AgentNetwork contributors={contributors} machines={agentMachines} tasks={tasks}
          selectedOwner={selected?.name ?? ""} onSelectOwner={setSelectedName}
          canRunLocally={canRunLocally} initialFocus={focusNetworkOwner} onOpenTask={onOpenTask} />
      )}

      {subView === "roster" &&
        (!selected ? (
          <div className="ct-empty">No contributors registered on this hub yet.</div>
        ) : (
          <div className="ct-workspace">
            <section className="ct-roster" aria-labelledby="ct-roster-title">
              <h2 id="ct-roster-title">Project roster</h2>
              <div className="ct-column-headings" aria-hidden="true">
                <span>Contributor</span>
                <span>Operational seat</span>
                <span>Status</span>
              </div>
              <ul>
                {contributors.map((participant) => (
                  <li key={participant.name}>
                    <button
                      className="ct-roster-button"
                      type="button"
                      aria-pressed={selected.name === participant.name}
                      onClick={() => setSelectedName(participant.name)}
                    >
                      <span className="ct-roster-person">
                        <Avatar initials={participant.initials} person={participant.name} size="md" />
                        <span>
                          <strong>{participant.name}</strong>
                          <small className="ct-roster-caption">
                            <span>{participant.role}</span>
                          </small>
                        </span>
                      </span>
                      <span className="ct-roster-seat">{participant.seat}</span>
                      <StatusLabel
                        variant="dot"
                        tone={agentStateTone(participant.state)}
                        hollowDot={participant.state.toLowerCase() === "idle"}
                      >
                        {participant.state}
                      </StatusLabel>
                      <CaretRight className="ct-row-caret" size={14} />
                    </button>
                  </li>
                ))}
              </ul>
            </section>

            <aside className="ct-inspector" aria-label="Selected contributor">
              <header className="ct-inspector-header">
                <Avatar initials={selected.initials} person={selected.name} size="lg" />
                <div>
                  <h2>{selected.name}</h2>
                  <StatusLabel
                    variant="dot"
                    tone={agentStateTone(selected.state)}
                    hollowDot={selected.state.toLowerCase() === "idle"}
                  >
                    {selected.state}
                  </StatusLabel>
                </div>
              </header>

              <InspectorSection title="Access" icon={<ShieldCheck size={17} />}>
                <DefinitionList
                  items={[
                    { label: "Membership", value: selected.role },
                    { label: "Operational seat", value: selected.seat },
                    {
                      label: "Trust",
                      value: <span className="ct-tier">Tier {selected.tier}</span>,
                    },
                    ...(selected.keyId
                      ? [
                          {
                            label: "Key",
                            value: (
                              <span className="mono">
                                <Key size={13} style={{ verticalAlign: "-2px", marginRight: 4 }} />
                                {selected.keyId.slice(0, 10)} · {selected.keyState ?? "active"}
                              </span>
                            ),
                          },
                        ]
                      : []),
                  ]}
                />
              </InspectorSection>

              <InspectorSection title="Agent capacity" icon={<Circuitry size={17} />}>
                <DefinitionList items={[
                  { label: "Machines", value: selectedMachines.length },
                  { label: "Reported agents", value: selectedAgents.length },
                ]} />
                <Button variant="link" className="ct-agent-map-link" onClick={() => { setFocusNetworkOwner(true); setSubView("agents"); }}>
                  View in network <ArrowRight size={15} />
                </Button>
              </InspectorSection>

              {(selected.stamps?.length || selected.missingTier?.length || selected.openPenalties?.length) ? (
                <InspectorSection title="Standing stamps" icon={<Stamp size={17} />}>
                  {selected.stamps && selected.stamps.length > 0 && (
                    <ul className="ct-stamps">
                      {selected.stamps.map((stamp, idx) => (
                        <li key={`${stamp.kind}-${idx}`}>
                          <StatusLabel variant="chip" tone="good">
                            {stamp.kind}
                          </StatusLabel>
                          <span className="mono">{stamp.evidence}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                  {selected.openPenalties && selected.openPenalties.length > 0 && (
                    <ul className="ct-stamps">
                      {selected.openPenalties.map((p) => (
                        <li key={p.id}>
                          <StatusLabel variant="chip" tone="danger">
                            {p.kind}
                          </StatusLabel>
                          <span className="mono">#{p.id}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                  {selected.missingTier && selected.missingTier.length > 0 && (
                    <p className="ct-capacity-note">
                      <Info size={15} /> Next tier needs: {selected.missingTier.join(", ")}
                    </p>
                  )}
                </InspectorSection>
              ) : null}
            </aside>
          </div>
        ))}

      {subView === "pools" && (
        <div className="ct-pools-grid">
          {pools.length === 0 ? (
            <div className="ct-empty">No quota pools reported yet.</div>
          ) : (
            pools.map((pool) => (
              <MeterBar key={`${pool.owner}-${pool.pool}`} pool={pool} onSelectTask={onOpenTask} />
            ))
          )}
        </div>
      )}

      {subView === "roles" && (
        <div className="ct-roles-grid">
          {roleLeases.length === 0 ? (
            <div className="ct-empty">No role leases configured on this hub.</div>
          ) : (
            roleLeases.map((lease) => (
              <InspectorPanel
                key={`${lease.role}-${lease.scope}`}
                mode="docked"
                eyebrow={<span className="mono">{lease.scope}</span>}
                title={lease.role}
              >
                <DefinitionList
                  items={[
                    {
                      label: "Lease state",
                      value: (
                        <StatusLabel
                          variant="pill"
                          tone={lease.state === "held" ? "good" : lease.state === "handing_over" ? "warning" : "muted"}
                        >
                          {lease.state.replaceAll("_", " ")}
                        </StatusLabel>
                      ),
                    },
                    { label: "Holder", value: lease.holder ?? "Vacant" },
                    { label: "Lease scope", value: lease.leaseScope },
                    { label: "Epoch", value: <span className="mono">#{lease.epoch}</span> },
                    { label: "Reason", value: lease.lastReason || "—" },
                  ]}
                />
              </InspectorPanel>
            ))
          )}
        </div>
      )}

      {subView !== "agents" && <RoleGuide />}
    </section>
  );
}
