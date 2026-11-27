import { useRef, useState } from "react";
import { ArrowRight, ArrowSquareOut, Check, Copy, Desktop, HardDrives, Info, Robot, TerminalWindow, X } from "@phosphor-icons/react";
import type { AgentInstance, AgentMachine, Contributor, WorkTask } from "../api/types";
import { Avatar, Button, CodeChip, DefinitionList, FilterBar, FilterField, InspectorPanel, ProviderLabel, ProviderMark, StatusLabel, agentStateTone } from "../components";
import ParticipantTopology from "./ParticipantTopology";
import "./agent-network.css";

const launchCommand = "sommelier agent run --config agent.toml";

type Props = {
  contributors: Contributor[];
  machines: AgentMachine[];
  tasks: WorkTask[];
  selectedOwner: string;
  onSelectOwner: (name: string) => void;
  canRunLocally: boolean;
  initialFocus: boolean;
  onOpenTask: (id: string) => void;
};

function AgentStatus({ state }: { state: string }) {
  return (
    <StatusLabel variant="dot" tone={agentStateTone(state)} hollowDot={state.toLowerCase() === "idle"}>
      {state}
    </StatusLabel>
  );
}

function AgentGlyph({ agent }: { agent: AgentInstance }) {
  return agent.provider ? <ProviderMark provider={agent.provider} size={18} /> : <Robot size={18} aria-hidden="true" />;
}

function agentSubtitle(worker: AgentInstance): string {
  const family =
    worker.provider ??
    (worker.agentClass && worker.agentClass.toLowerCase() !== "default" ? worker.agentClass : "Agent");
  return worker.taskId ? `${family} · ${worker.taskId}` : family;
}

function LocalAgentSetup() {
  const dialog = useRef<HTMLDialogElement>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "failed">("idle");

  async function copyCommand() {
    try {
      await navigator.clipboard.writeText(launchCommand);
      setCopyState("done");
    } catch {
      setCopyState("failed");
    }
  }

  return (
    <>
      <Button size="sm" onClick={() => { setCopyState("idle"); dialog.current?.showModal(); }}>
        <TerminalWindow size={16} /> Run locally
      </Button>
      <dialog ref={dialog} className="app-dialog agent-setup" aria-labelledby="agent-setup-title">
        <div className="dialog-heading">
          <h2 id="agent-setup-title">Run an agent locally</h2>
          <Button variant="icon" aria-label="Close local setup" onClick={() => dialog.current?.close()}>
            <X size={18} />
          </Button>
        </div>
        <ol>
          <li>Install Sommelier and sign in to this hub from the machine you want to contribute.</li>
          <li>Configure the local supervisor with the agent class and capacity you want to offer. Your model provider login stays on that machine.</li>
          <li>Run the command with your local config. The supervisor automatically reports heartbeats, liveness, and task progress.</li>
        </ol>
        <div className="agent-setup-command">
          <code>{launchCommand}</code>
          <Button size="sm" onClick={copyCommand}>
            {copyState === "done" ? <Check size={15} /> : <Copy size={15} />} {copyState === "done" ? "Copied" : "Copy"}
          </Button>
        </div>
        <p role="status">
          {copyState === "failed"
            ? "Clipboard unavailable. Select and copy the command."
            : copyState === "done"
              ? "Command copied."
              : ""}
        </p>
        <a href="https://github.com/decantlabs/sommelier/blob/main/bus/docs/JOIN.md" target="_blank" rel="noreferrer">
          Setup guide <ArrowSquareOut size={15} />
        </a>
      </dialog>
    </>
  );
}

export default function AgentNetwork({
  contributors,
  machines,
  tasks,
  selectedOwner,
  onSelectOwner,
  canRunLocally,
  initialFocus,
  onOpenTask,
}: Props) {
  const [selectedAgentName, setSelectedAgentName] = useState<string | null>(null);
  const [showDetails, setShowDetails] = useState(initialFocus);
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState("all");
  const [page, setPage] = useState(0);

  const selected = contributors.find(({ name }) => name === selectedOwner);
  const hosts = machines.filter(({ owner }) => owner === selectedOwner);
  const agents = hosts.flatMap(({ agents }) => agents);
  const selectedHost = hosts.find(({ agents }) => agents.some(({ name }) => name === selectedAgentName));
  const agent = selectedHost?.agents.find(({ name }) => name === selectedAgentName);
  const task = agent?.taskId ? tasks.find(({ id }) => id === agent.taskId) : undefined;
  const running = agents.filter(({ state }) => state.toLowerCase() === "running").length;

  const summary = new Map<string, AgentInstance[]>();
  for (const host of machines) {
    const entry = summary.get(host.owner) ?? [];
    entry.push(...host.agents);
    summary.set(host.owner, entry);
  }
  const runningTotal = machines.reduce(
    (count, host) => count + host.agents.filter(({ state }) => state === "Running").length,
    0,
  );
  const attentionStates = new Set(["Blocked", "Dead", "Draining", "Offline", "Starved", "Stale", "Suspect"]);

  const matching = contributors.filter((owner) => {
    const ownerAgents = summary.get(owner.name) ?? [];
    const matchesQuery = !query.trim() || owner.name.toLowerCase().includes(query.trim().toLowerCase());
    return (
      matchesQuery &&
      (filter === "all" ||
        (filter === "running" && ownerAgents.some(({ state }) => state === "Running")) ||
        (filter === "attention" && ownerAgents.some(({ state }) => attentionStates.has(state))) ||
        (filter === "empty" && !ownerAgents.length))
    );
  });

  const pageSize = 24;
  const pageCount = Math.max(1, Math.ceil(matching.length / pageSize));
  const currentPage = Math.min(page, pageCount - 1);
  const visible = matching.slice(currentPage * pageSize, (currentPage + 1) * pageSize);

  const nodes = visible.map((owner) => {
    const owned = summary.get(owner.name) ?? [];
    const warnings = owned.filter(({ state }) => attentionStates.has(state)).length;
    const active = owned.filter(({ state }) => state === "Running").length;
    const isPressed = showDetails && selectedOwner === owner.name;
    return {
      id: owner.name,
      tone: warnings > 0 ? ("attention" as const) : active > 0 ? ("running" as const) : ("quiet" as const),
      button: (
        <button
          type="button"
          className="agent-owner"
          aria-pressed={isPressed}
          title={
            warnings > 0
              ? `${owner.name} · ${warnings} ${warnings === 1 ? "agent" : "agents"} needing attention`
              : active > 0
                ? `${owner.name} · ${active} running`
                : owner.name
          }
          aria-controls={isPressed ? "network-inspector" : undefined}
          onClick={() => {
            onSelectOwner(owner.name);
            setSelectedAgentName(null);
            setShowDetails(true);
          }}
        >
          <Avatar initials={owner.initials} person={owner.name} size="sm" />
          <span className="agent-owner-copy">
            <strong>{owner.name}</strong>
            <small>
              {owned.length} {owned.length === 1 ? "agent" : "agents"}
            </small>
          </span>
          {warnings > 0 ? (
            <StatusLabel
              variant="pill"
              tone="warning"
              className="network-node-badge"
            >
              {warnings} suspect
            </StatusLabel>
          ) : active > 0 ? (
            <StatusLabel
              variant="pill"
              tone="good"
              className="network-node-badge"
            >
              {active} live
            </StatusLabel>
          ) : (
            <ArrowRight size={14} aria-hidden="true" />
          )}
        </button>
      ),
    };
  });

  return (
    <div className="agent-network">
      <FilterBar ariaLabel="Agent network filters">
        <FilterField label="Find participant">
          <input
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
              setPage(0);
            }}
            placeholder="Participant name"
          />
        </FilterField>
        <FilterField label="Agents">
          <select
            value={filter}
            onChange={(event) => {
              setFilter(event.target.value);
              setPage(0);
            }}
          >
            <option value="all">All participants</option>
            <option value="running">Running</option>
            <option value="attention">Needs attention</option>
            <option value="empty">No agents reported</option>
          </select>
        </FilterField>
        {canRunLocally && <LocalAgentSetup />}
      </FilterBar>

      <div className={`agent-network-stage ${showDetails && selected ? "has-inspector" : ""}`}>
        <section className="network-canvas" aria-label="Participant network">
          <ParticipantTopology
            nodes={nodes}
            selected={showDetails ? selectedOwner : undefined}
            hub={
              <div className="network-hub">
                <span className="agent-icon">
                  <HardDrives size={20} aria-hidden="true" />
                </span>
                <div>
                  <strong>Factory hub</strong>
                  <small>
                    {contributors.length} participants · {runningTotal} running
                  </small>
                </div>
              </div>
            }
          />
          {!visible.length && <p className="agent-empty network-no-results">No participants match these filters.</p>}
          <div className="network-footer">
            <span>
              {matching.length
                ? `${currentPage * pageSize + 1}–${Math.min((currentPage + 1) * pageSize, matching.length)} of ${matching.length} participants`
                : "0 participants"}
            </span>
            {pageCount > 1 && (
              <span>
                <Button size="sm" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>
                  Previous
                </Button>
                <Button size="sm" disabled={currentPage + 1 >= pageCount} onClick={() => setPage(currentPage + 1)}>
                  Next
                </Button>
              </span>
            )}
          </div>
        </section>

        {showDetails && selected && (
          <div id="network-inspector" className="network-inspector-pane">
            {!agent && (
              <section className="agent-canvas" aria-label={`Machines and agents for ${selected.name}`}>
                <div className="agent-map-heading">
                  <div>
                    <p>Participant details</p>
                    <h2>{selected.name}</h2>
                  </div>
                  <Button
                    variant="icon"
                    aria-label="Close participant details"
                    onClick={() => setShowDetails(false)}
                  >
                    <X size={17} />
                  </Button>
                </div>
                <div className="agent-root">
                  <Avatar initials={selected.initials} person={selected.name} size="md" />
                  <div>
                    <strong>{selected.name}</strong>
                    <small>
                      {hosts.length} {hosts.length === 1 ? "host" : "hosts"} · {agents.length}{" "}
                      {agents.length === 1 ? "agent" : "agents"} · {running} running
                    </small>
                  </div>
                </div>
                {hosts.length ? (
                  <ul className="agent-tree">
                    {hosts.map((host) => (
                      <li key={host.name ?? ""} className="agent-machine">
                        <div className="agent-machine-head">
                          <span className="agent-icon">
                            <Desktop size={18} aria-hidden="true" />
                          </span>
                          <div>
                            <h3>{host.name ?? "Unmanaged session"}</h3>
                            <p>
                              {host.slots !== undefined ? `${host.slots} declared slots · ` : ""}
                              {host.agents.length} {host.agents.length === 1 ? "agent" : "agents"}
                            </p>
                          </div>
                        </div>
                        {host.agents.length ? (
                          <ul className="agent-branches">
                            {host.agents.map((worker) => (
                              <li key={worker.name}>
                                <button
                                  type="button"
                                  className="agent-node"
                                  onClick={() => setSelectedAgentName(worker.name)}
                                >
                                  <span className="agent-icon">
                                    <AgentGlyph agent={worker} />
                                  </span>
                                  <span className="agent-node-name">
                                    <strong>{worker.name}</strong>
                                    <small>{agentSubtitle(worker)}</small>
                                  </span>
                                  <AgentStatus state={worker.state} />
                                </button>
                              </li>
                            ))}
                          </ul>
                        ) : (
                          <p className="agent-machine-empty">No agent has reported from this host.</p>
                        )}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="agent-empty">No hosts or agents have reported for {selected.name}.</p>
                )}
              </section>
            )}

            {agent && (
              <InspectorPanel
                className="agent-inspector"
                ariaLabel="Agent details"
                eyebrow={
                  <>
                    <Robot size={16} /> Agent details
                  </>
                }
                title={agent.name}
                onClose={() => setSelectedAgentName(null)}
                closeLabel="Close agent details"
              >
                <div className="agent-inspector-status">
                  <AgentStatus state={agent.state} />
                  <Button variant="link" onClick={() => setSelectedAgentName(null)}>
                    Back to hosts
                  </Button>
                </div>
                <DefinitionList
                  items={[
                    { label: "Owner", value: selectedOwner },
                    { label: "Host", value: selectedHost?.name ?? "Unmanaged session" },
                    {
                      label: "Liveness",
                      value:
                        agent.liveness === "process" ? (
                          <StatusLabel variant="pill" tone="good">
                            OS process{agent.pid ? ` · PID ${agent.pid}` : ""}
                          </StatusLabel>
                        ) : (
                          <StatusLabel variant="pill" tone="muted">
                            Self-reported
                          </StatusLabel>
                        ),
                    },
                    ...(agent.provider ? [{ label: "Provider", value: <ProviderLabel provider={agent.provider} /> }] : []),
                    ...(agent.model ? [{ label: "Model", value: agent.model }] : []),
                    ...(agent.agentClass ? [{ label: "Class", value: <CodeChip small>{agent.agentClass}</CodeChip> }] : []),
                    ...(agent.mode
                      ? [{ label: "Assignment", value: agent.mode === "Auto" ? "Auto · direct lease" : "Own · offers" }]
                      : []),
                    ...(agent.lastStatus ? [{ label: "Last status", value: agent.lastStatus }] : []),
                    ...(agent.branch ? [{ label: "Branch", value: <CodeChip small>{agent.branch}</CodeChip> }] : []),
                  ]}
                />
                {agent.reason && <p className="agent-warning">{agent.reason}</p>}
                {agent.liveness !== "process" && (
                  <p className="agent-liveness-hint">
                    <Info size={14} /> No OS process supervisor is attached to verify liveness automatically. Launch via <code>sommelier agent run</code> or <code>agent exec</code> for automatic process heartbeats.
                  </p>
                )}
                {agent.statusDetail && <p className="agent-status-detail">{agent.statusDetail}</p>}
                {agent.taskId && (
                  <div className="agent-inspector-task">
                    {task ? (
                      <Button variant="link" onClick={() => onOpenTask(task.id)}>
                        <span>
                          <code>{task.id}</code> {task.title}
                        </span>
                        <ArrowRight size={15} />
                      </Button>
                    ) : (
                      <CodeChip small>{agent.taskId}</CodeChip>
                    )}
                  </div>
                )}
              </InspectorPanel>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
