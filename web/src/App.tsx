import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import {
  ArrowSquareOut,
  Broadcast,
  CaretDown,
  Check,
  Info,
  Lock,
  MagnifyingGlass,
  Plus,
  Sparkle,
  WarningCircle,
  X,
} from "@phosphor-icons/react";
import { initialTasks } from "./api/demo";
import { useSommelierData, type BriefResult } from "./api/client";
import type { AccessRole, AgentProvider } from "./api/types";
import { Button, DefinitionList, MarkdownBrief, ProviderMark } from "./components";
import WorkGraphView from "./views/WorkGraphView";
import PipelineView from "./views/PipelineView";
import LoopsView from "./views/LoopsView";
import TestingView from "./views/TestingView";
import MessageBoardView from "./views/MessageBoardView";
import ContributorsView from "./views/ContributorsView";

export type Tab = "Graph" | "Loops" | "Pipeline" | "Testing" | "Message board" | "Contributors";
const tabs: Tab[] = ["Graph", "Loops", "Pipeline", "Testing", "Message board", "Contributors"];
const roles: AccessRole[] = ["Orchestrator", "Member", "Observer", "Public"];

type NoticeState = {
  text: string;
  tone: "good" | "info" | "warning" | "danger";
};

function initialsFor(name: string): string {
  const clean = name.replace(/^[^a-zA-Z0-9]+/, "");
  if (clean.length >= 2) return clean.slice(0, 2).toUpperCase();
  return (clean || "SM").toUpperCase();
}

export function App() {
  const [tab, setTab] = useState<Tab>("Graph");
  const [previewRole, setPreviewRole] = useState<AccessRole>("Orchestrator");
  const data = useSommelierData(previewRole);
  const { snapshot, source, identity, effectiveRole, lastError, clearError } = data;

  const [selectedTaskId, setSelectedTaskId] = useState<string | null>("SOM-142");
  const [selectedLoopTask, setSelectedLoopTask] = useState<string | null>(null);
  const [selectedSubmissionTask, setSelectedSubmissionTask] = useState<string | null>(null);
  const [evidenceTaskId, setEvidenceTaskId] = useState<string>("SOM-138");
  const [scope, setScope] = useState("");
  const [searchOpen, setSearchOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [needsAttention, setNeedsAttention] = useState(false);
  const [topic, setTopic] = useState("");
  const [menu, setMenu] = useState<"hosting" | "role" | "project" | null>(null);
  const [notice, setNotice] = useState<NoticeState | null>(null);
  const [briefTitle, setBriefTitle] = useState("");
  const [briefResult, setBriefResult] = useState<BriefResult>({ status: "ok", content: "" });
  const [tokenInput, setTokenInput] = useState("");

  const createDialog = useRef<HTMLDialogElement>(null);
  const evidenceDialog = useRef<HTMLDialogElement>(null);
  const briefDialog = useRef<HTMLDialogElement>(null);
  const noticeTimerRef = useRef<number | null>(null);

  const showNotice = (text: string, tone: NoticeState["tone"] = "good") => {
    if (noticeTimerRef.current) window.clearTimeout(noticeTimerRef.current);
    setNotice({ text, tone });
    noticeTimerRef.current = window.setTimeout(() => setNotice(null), 3600);
  };

  // Auto-select the first relevant task when switching from demo to a live hub.
  useEffect(() => {
    if (selectedTaskId === null || snapshot.tasks.some((t) => t.id === selectedTaskId)) return;
    if (snapshot.tasks.length > 0) {
      const firstRelevant =
        snapshot.tasks.find((t) => Boolean(t.conflict)) ??
        snapshot.tasks.find((t) => t.state !== "Cancelled" && t.state !== "Closed") ??
        snapshot.tasks[0];
      setSelectedTaskId(firstRelevant.id);
    } else {
      setSelectedTaskId(null);
    }
  }, [selectedTaskId, snapshot.tasks]);

  useEffect(() => {
    if (!lastError) return;
    showNotice(lastError, "warning");
    clearError();
  }, [clearError, lastError]);

  const agents = snapshot.agentMachines.flatMap(({ agents }) => agents);
  const communityProviders = [
    ...new Set(
      agents
        .map(({ provider }) => provider)
        .filter((provider): provider is AgentProvider => Boolean(provider)),
    ),
  ];
  const runningAgentCount = agents.filter(({ state }) => state.toLowerCase() === "running").length;
  const currentUser = source === "live" ? identity?.name ?? "" : "mira";
  const canRunLocally =
    (effectiveRole === "Member" || effectiveRole === "Orchestrator") &&
    (identity?.tier === undefined || identity.tier >= 1);
  const recipientNames = snapshot.contributors
    .map((c) => c.name)
    .filter((name) => name !== currentUser);

  const liveHost =
    typeof window !== "undefined" && window.location.host ? window.location.host : "live hub";
  const openAlertCount = snapshot.alerts.filter((a) => !a.acked).length;
  const pipelineCount =
    snapshot.submissions.length + snapshot.reviews.length + snapshot.landing.length;

  const tabCounts: Partial<Record<Tab, number>> = {
    Graph: snapshot.tasks.length,
    Pipeline: pipelineCount,
    "Message board": snapshot.messages.length,
    Contributors: snapshot.contributors.length,
  };

  function go(next: Tab) {
    setTab(next);
    setMenu(null);
  }

  function openMessages(nextTopic: string) {
    setTopic(nextTopic);
    go("Message board");
  }

  function openSubmission(taskId: string) {
    setSelectedSubmissionTask(taskId);
    go("Pipeline");
  }

  function openTask(id: string) {
    if (!snapshot.tasks.some((task) => task.id === id)) {
      if (snapshot.submissions.some((s) => s.task === id) || id === "SOM-138") {
        setEvidenceTaskId(id);
        evidenceDialog.current?.showModal();
      }
      return;
    }
    setSelectedTaskId(id);
    setSearch("");
    setScope("");
    setNeedsAttention(false);
    go("Graph");
  }

  async function inspectBrief(taskId: string) {
    const result = await data.fetchBrief(taskId);
    setBriefTitle(taskId);
    setBriefResult(result);
    briefDialog.current?.showModal();
  }

  function handleCreateTask(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (effectiveRole !== "Orchestrator") return;
    const form = event.currentTarget;
    const formData = new FormData(form);
    const value = (key: string) => String(formData.get(key) ?? "").trim();
    if (!value("title") || !value("scope") || !value("done")) {
      showNotice("Enter a title, scope and acceptance criteria.", "warning");
      return;
    }
    const created = data.createTask({
      title: value("title"),
      scope: value("scope"),
      done: value("done"),
      priority: value("priority"),
      state: value("state") === "Ready" ? "Ready" : "Draft",
      kind: value("kind") || "fix",
      risk: (value("risk") as "low" | "normal" | "guarded") || "normal",
    });
    if (!created) return;
    setSelectedTaskId(created.id);
    setSearch("");
    setScope("");
    setNeedsAttention(false);
    form.reset();
    createDialog.current?.close();
    showNotice(
      source === "live" ? `${created.id} sent to hub` : `${created.id} created in this demo`,
      "good",
    );
  }

  async function handleConnectLive(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const ok = await data.connectLive(tokenInput);
    if (ok) {
      setTokenInput("");
      setMenu(null);
      showNotice("Connected to live Sommelier hub", "good");
    } else {
      showNotice("Hub unreachable or token rejected · staying in demo mode", "warning");
    }
  }

  const evidenceSub = snapshot.submissions.find((s) => s.task === evidenceTaskId);
  const evidenceReviews = snapshot.reviews.filter((r) => r.task === evidenceTaskId);
  const activeTask = snapshot.tasks.find((t) => t.id === selectedTaskId);
  const avatarInitials =
    source === "live" && identity
      ? initialsFor(identity.name)
      : effectiveRole === "Public"
        ? "PU"
        : "MI";

  return (
    <main className="shell">
      <header className="app-header">
        <div className="brand-group">
          <button
            type="button"
            className="brand"
            onClick={() => go("Graph")}
            aria-label="Sommelier work graph"
          >
            <img src={`${import.meta.env.BASE_URL}assets/sommelier-mark.svg`} width="28" height="28" alt="" />
            <strong>sommelier</strong>
          </button>
          <span className="slash">/</span>
          <div className="menu-anchor">
            <button
              type="button"
              className="project-switch"
              onClick={() => setMenu(menu === "project" ? null : "project")}
              aria-expanded={menu === "project"}
            >
              decantlabs / sommelier <CaretDown size={13} />
            </button>
            {menu === "project" && (
              <div className="menu project-menu">
                <span className="menu-label">
                  {source === "live" ? `Live hub #${snapshot.hubId}` : "Demo workspace"}
                </span>
                <a href="https://github.com/decantlabs/sommelier" target="_blank" rel="noreferrer">
                  Open source repository <ArrowSquareOut size={15} />
                </a>
              </div>
            )}
          </div>
        </div>

        <nav aria-label="Project views">
          {tabs.map((entry) => {
            const count = tabCounts[entry];
            return (
              <button
                key={entry}
                type="button"
                className={tab === entry ? "current" : ""}
                aria-current={tab === entry ? "page" : undefined}
                onClick={() => go(entry)}
              >
                <span>{entry === "Graph" ? "Work Graph" : entry}</span>
                {count !== undefined && count > 0 && (
                  <span className="nav-count mono">{count}</span>
                )}
                {entry === "Message board" && openAlertCount > 0 && (
                  <span className="nav-alert-dot" title={`${openAlertCount} open alerts`} />
                )}
              </button>
            );
          })}
        </nav>

        <div className="header-right">
          <div className="menu-anchor">
            <button
              type="button"
              className={`hosting ${source === "live" ? "is-live-pill" : ""}`}
              onClick={() => setMenu(menu === "hosting" ? null : "hosting")}
              aria-expanded={menu === "hosting"}
            >
              {source === "live" ? (
                <>
                  <span className="hosting-dot is-live" title="Connected to live hub" />
                  <span className="hosting-label">{liveHost}</span>
                </>
              ) : (
                <>
                  <Sparkle size={15} />
                  <span className="hosting-label">Demo preview</span>
                  {data.liveStatus === "auth-required" && (
                    <span className="hosting-dot is-auth" title="Live hub requires a token" />
                  )}
                </>
              )}
              <CaretDown size={12} />
            </button>
            {menu === "hosting" && (
              <div className="menu right">
                <span className="menu-label">
                  {source === "live" ? `Connected · Hub #${snapshot.hubId}` : "Demo mode"}
                </span>
                {source === "live" && (
                  <div className="menu-meta-line">
                    <Broadcast size={14} />
                    <span>
                      {effectiveRole} · updated {snapshot.asOf}
                    </span>
                  </div>
                )}
                <div className="menu-divider" />
                {data.liveStatus === "auth-required" && source !== "live" && (
                  <span className="menu-note">Live hub requires a token</span>
                )}
                <form className="menu-form" onSubmit={handleConnectLive}>
                  <label>
                    Hub token (Bearer → HttpOnly session)
                    <input
                      type="password"
                      value={tokenInput}
                      onChange={(e) => setTokenInput(e.target.value)}
                      placeholder="sb_... (empty for public read)"
                    />
                  </label>
                  <Button variant="primary" size="sm" type="submit">
                    {source === "live" ? "Sign in / reconnect" : "Connect live hub"}
                  </Button>
                  {source === "live" && (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => {
                        void data.switchToDemo();
                        setMenu(null);
                      }}
                    >
                      Switch to demo preview
                    </Button>
                  )}
                </form>
              </div>
            )}
          </div>

          <div className="menu-anchor">
            <button
              type="button"
              className="header-avatar"
              aria-label={
                source === "live" && identity
                  ? `Hub identity: ${identity.name} (${effectiveRole})`
                  : `Demo access role: ${effectiveRole}`
              }
              aria-expanded={menu === "role"}
              onClick={() => setMenu(menu === "role" ? null : "role")}
            >
              {avatarInitials}
            </button>
            {menu === "role" && (
              <div className="menu right">
                {source === "live" && identity ? (
                  <>
                    <span className="menu-label">
                      Live: {identity.name} · {effectiveRole}
                    </span>
                    {effectiveRole === "Public" && (
                      <button
                        type="button"
                        onClick={() => setMenu("hosting")}
                      >
                        Sign in with hub token
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => {
                        void data.switchToDemo();
                        setMenu(null);
                      }}
                    >
                      Sign out &amp; demo roles
                    </button>
                  </>
                ) : (
                  <>
                    <span className="menu-label">Preview an access role</span>
                    {roles.map((value) => (
                      <button
                        key={value}
                        type="button"
                        onClick={() => {
                          setPreviewRole(value);
                          setMenu(null);
                        }}
                      >
                        {value}
                        {previewRole === value && <Check size={14} />}
                      </button>
                    ))}
                  </>
                )}
              </div>
            )}
          </div>
        </div>
      </header>

      {tab === "Graph" ? (
        <div className="view graph-view">
          <div className="view-header">
            <div className="view-header-title">
              <h1>Work graph</h1>
              <span>
                {effectiveRole === "Public" ? "Public hub" : `${effectiveRole} session`} <b>·</b>{" "}
                {source === "live" ? `Event #${snapshot.hubId}` : "Demo project"}
              </span>
            </div>
            <div className="view-actions">
              {searchOpen && (
                <input
                  autoFocus
                  aria-label="Search tasks"
                  placeholder="Find a task, scope, or ID…"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                />
              )}
              <Button
                variant="icon"
                aria-label={searchOpen ? "Close search" : "Find tasks"}
                onClick={() => {
                  setSearchOpen(!searchOpen);
                  setSearch("");
                }}
              >
                {searchOpen ? <X size={19} /> : <MagnifyingGlass size={20} />}
              </Button>
              {effectiveRole === "Orchestrator" && (
                <Button
                  variant="primary"
                  size="sm"
                  onClick={() => {
                    setNotice(null);
                    createDialog.current?.showModal();
                  }}
                >
                  <Plus size={16} /> Create task
                </Button>
              )}
            </div>
          </div>

          <WorkGraphView
            tasks={snapshot.tasks}
            messages={snapshot.messages}
            selected={activeTask}
            onSelect={setSelectedTaskId}
            onMessages={openMessages}
            onInspectBrief={inspectBrief}
            onOpenSubmission={openSubmission}
            onOpenLoop={(taskId) => {
              setSelectedLoopTask(taskId);
              go("Loops");
            }}
            submissions={snapshot.submissions}
            alerts={snapshot.alerts}
            scheduler={snapshot.scheduler}
            role={effectiveRole}
            onAckAlert={data.ackAlert}
            onTogglePause={data.togglePauseAll}
            onOpenTask={openTask}
            onOpenContributors={() => go("Contributors")}
            onOpenPipeline={() => go("Pipeline")}
            search={search}
            scope={scope}
            onScopeChange={setScope}
            needsAttention={needsAttention}
            onNeedsAttentionChange={setNeedsAttention}
            presenceProviders={communityProviders.map((provider) => (
              <span key={provider} title={provider}>
                <ProviderMark provider={provider} size={15} />
              </span>
            ))}
            runningAgentCount={runningAgentCount}
          />
        </div>
      ) : null}

      {tab === "Loops" && (
        <div className="view">
          <LoopsView
            tasks={snapshot.tasks}
            submissions={snapshot.submissions}
            initialTaskId={selectedLoopTask}
            attempts={snapshot.workAttempts}
            onTask={openTask}
            onPipeline={openSubmission}
          />
        </div>
      )}

      {tab === "Pipeline" && (
        <div className="view">
          <PipelineView
            submissions={snapshot.submissions}
            reviews={snapshot.reviews}
            landing={snapshot.landing}
            batches={snapshot.batches}
            initialTaskFilter={selectedSubmissionTask}
            onClearTaskFilter={() => setSelectedSubmissionTask(null)}
            onOpenTask={openTask}
            onMessages={openMessages}
          />
        </div>
      )}

      {tab === "Testing" && (
        <div className="view">
          <TestingView
            features={snapshot.features}
            matrix={snapshot.matrix}
            matrixBlocksNote={snapshot.matrixBlocksNote}
            runOrders={snapshot.runOrders}
            regressions={snapshot.regressions}
            onOpenTask={openTask}
            onMessages={openMessages}
          />
        </div>
      )}

      <div className="view board-view" hidden={tab !== "Message board"}>
        <MessageBoardView
          messages={snapshot.messages}
          currentUser={currentUser}
          recipients={recipientNames}
          role={effectiveRole}
          source={source}
          topic={topic}
          setTopic={setTopic}
          onTask={openTask}
          onPublish={data.postMessage}
        />
      </div>

      {tab === "Contributors" && (
        <div className="view">
          <ContributorsView
            contributors={snapshot.contributors}
            agentMachines={snapshot.agentMachines}
            tasks={snapshot.tasks}
            currentUser={currentUser}
            canRunLocally={canRunLocally}
            pools={snapshot.pools}
            roleLeases={snapshot.roleLeases}
            onOpenTask={openTask}
          />
        </div>
      )}

      <dialog ref={createDialog} className="app-dialog">
        <form onSubmit={handleCreateTask}>
          <div className="dialog-heading">
            <h2>Create task</h2>
            <Button
              variant="icon"
              aria-label="Close create task"
              onClick={() => createDialog.current?.close()}
            >
              <X size={19} />
            </Button>
          </div>
          <label>
            Title
            <input autoFocus name="title" required maxLength={85} placeholder="What needs to be done?" />
          </label>
          <label>
            Scope
            <input name="scope" required placeholder="bus/src/agent/**" />
          </label>
          <label>
            Done when
            <textarea name="done" required placeholder="Describe the acceptance criteria" />
          </label>
          <div className="dialog-columns">
            <label>
              Priority
              <select name="priority" defaultValue="Normal">
                <option>Blocker</option>
                <option>High</option>
                <option>Normal</option>
                <option>Filler</option>
              </select>
            </label>
            <label>
              Initial state
              <select name="state" defaultValue="Draft">
                <option>Draft</option>
                <option>Ready</option>
              </select>
            </label>
          </div>
          <div className="dialog-columns">
            <label>
              Order kind
              <select name="kind" defaultValue="fix">
                <option value="fix">fix</option>
                <option value="review">review</option>
                <option value="scout">scout</option>
                <option value="run">run</option>
              </select>
            </label>
            <label>
              Risk
              <select name="risk" defaultValue="normal">
                <option value="low">low</option>
                <option value="normal">normal</option>
                <option value="guarded">guarded</option>
              </select>
            </label>
          </div>
          {notice && <p role="status">{notice.text}</p>}
          <Button variant="primary" type="submit">
            <Plus size={17} /> Create task
          </Button>
        </form>
      </dialog>

      <dialog ref={evidenceDialog} className="app-dialog">
        <div className="dialog-heading">
          <span className="task-id">{evidenceTaskId}</span>
          <Button
            variant="icon"
            aria-label="Close submission"
            onClick={() => evidenceDialog.current?.close()}
          >
            <X size={19} />
          </Button>
        </div>
        <h2>{evidenceSub ? `Submission ${evidenceSub.shortSha}` : "Verify signed review responses"}</h2>
        <DefinitionList
          items={[
            { label: "Submission", value: evidenceSub ? evidenceSub.state.replaceAll("_", " ") : "Reviewing" },
            { label: "Member", value: evidenceSub?.member ?? "—" },
            {
              label: "Clean build",
              value: evidenceSub?.verifyRunId
                ? `${evidenceSub.verifyConclusion ?? evidenceSub.verifyState ?? "Passed"} · run-${evidenceSub.verifyRunId}`
                : "Pending",
            },
            {
              label: "Reviews",
              value:
                evidenceReviews.length > 0
                  ? `${evidenceReviews.length} assigned (${evidenceReviews.map((r) => r.reviewer).join(", ")})`
                  : "2 distinct passes",
            },
            ...(evidenceSub?.auditAssignedTo
              ? [{ label: "Audit", value: `Assigned to ${evidenceSub.auditAssignedTo}` }]
              : []),
          ]}
        />
        <div className="ui-inspector-actions">
          <Button
            variant="primary"
            onClick={() => {
              evidenceDialog.current?.close();
              openMessages(`review/${evidenceTaskId}/*`);
            }}
          >
            Open review events
          </Button>
          <Button
            variant="secondary"
            onClick={() => {
              evidenceDialog.current?.close();
              openSubmission(evidenceTaskId);
            }}
          >
            Open in Pipeline
          </Button>
        </div>
      </dialog>

      <dialog ref={briefDialog} className="app-dialog brief-dialog">
        <div className="dialog-heading">
          <span className="task-id">Task brief · {briefTitle}</span>
          <Button variant="icon" aria-label="Close brief" onClick={() => briefDialog.current?.close()}>
            <X size={19} />
          </Button>
        </div>
        {briefResult.status === "auth-required" ? (
          <div className="brief-auth-callout">
            <Lock size={20} />
            <div>
              <strong>Sign in required</strong>
              <p>{briefResult.content}</p>
              <Button
                variant="primary"
                size="sm"
                onClick={() => {
                  briefDialog.current?.close();
                  setMenu("hosting");
                }}
              >
                Connect with hub token
              </Button>
            </div>
          </div>
        ) : briefResult.status === "not-found" ? (
          <div className="brief-empty-callout">
            <Info size={18} />
            <p>{briefResult.content}</p>
          </div>
        ) : (
          <MarkdownBrief content={briefResult.content} />
        )}
      </dialog>

      {notice && !createDialog.current?.open && (
        <div className={`toast toast-${notice.tone}`} role="status">
          {notice.tone === "warning" || notice.tone === "danger" ? (
            <WarningCircle size={17} />
          ) : notice.tone === "info" ? (
            <Info size={17} />
          ) : (
            <Check size={17} />
          )}
          <span>{notice.text}</span>
          <button type="button" className="toast-close" aria-label="Dismiss notice" onClick={() => setNotice(null)}>
            <X size={14} />
          </button>
        </div>
      )}

      <span className="sr-only" aria-hidden="true">
        {initialTasks.length}
      </span>
    </main>
  );
}
