import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import {
  ArrowSquareOut,
  CaretDown,
  Check,
  Cloud,
  MagnifyingGlass,
  Plus,
  Stack,
  X,
} from "@phosphor-icons/react";
import { initialTasks } from "./api/demo";
import { useSommelierData } from "./api/client";
import type { AccessRole } from "./api/types";
import { Button, DefinitionList, ProviderMark } from "./components";
import WorkGraphView from "./views/WorkGraphView";
import PipelineView from "./views/PipelineView";
import TestingView from "./views/TestingView";
import MessageBoardView from "./views/MessageBoardView";
import ContributorsView from "./views/ContributorsView";

export type Tab = "Graph" | "Pipeline" | "Testing" | "Message board" | "Contributors";
const tabs: Tab[] = ["Graph", "Pipeline", "Testing", "Message board", "Contributors"];
const roles: AccessRole[] = ["Orchestrator", "Member", "Observer", "Public"];

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
  const [selectedSubmissionTask, setSelectedSubmissionTask] = useState<string | null>(null);
  const [evidenceTaskId, setEvidenceTaskId] = useState<string>("SOM-138");
  const [scope, setScope] = useState("");
  const [searchOpen, setSearchOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [needsAttention, setNeedsAttention] = useState(false);
  const [topic, setTopic] = useState("");
  const [hosting, setHosting] = useState("Self-hosted");
  const [menu, setMenu] = useState<"hosting" | "role" | "project" | null>(null);
  const [notice, setNotice] = useState("");
  const [briefTitle, setBriefTitle] = useState("");
  const [briefText, setBriefText] = useState("");
  const [tokenInput, setTokenInput] = useState("");

  const createDialog = useRef<HTMLDialogElement>(null);
  const evidenceDialog = useRef<HTMLDialogElement>(null);
  const briefDialog = useRef<HTMLDialogElement>(null);

  // When switching from demo to a live hub, auto-select the first relevant task if SOM-142 isn't present.
  useEffect(() => {
    if (selectedTaskId && snapshot.tasks.some((t) => t.id === selectedTaskId)) return;
    if (snapshot.tasks.length > 0) {
      const firstRelevant =
        snapshot.tasks.find((t) => Boolean(t.conflict)) ?? snapshot.tasks[0];
      setSelectedTaskId(firstRelevant.id);
    } else {
      setSelectedTaskId(null);
    }
  }, [selectedTaskId, snapshot.tasks]);

  useEffect(() => {
    if (!lastError) return;
    setNotice(lastError);
    const timer = window.setTimeout(() => {
      setNotice("");
      clearError();
    }, 4200);
    return () => window.clearTimeout(timer);
  }, [clearError, lastError]);

  const communityProviders = [...new Set(snapshot.contributors.map(({ provider }) => provider))];
  const runningAgentCount = snapshot.contributors.filter(
    ({ state }) => state.toLowerCase() === "running",
  ).length;

  const currentUser = source === "live" && identity ? identity.name : "mira";
  const recipientNames = snapshot.contributors
    .map((c) => c.name)
    .filter((name) => name !== currentUser);

  function showNotice(text: string) {
    setNotice(text);
    window.setTimeout(() => setNotice(""), 3200);
  }

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
    const content = await data.fetchBrief(taskId);
    setBriefTitle(`Task brief · ${taskId}`);
    setBriefText(content);
    briefDialog.current?.showModal();
  }

  function handleCreateTask(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (effectiveRole !== "Orchestrator") return;
    const form = event.currentTarget;
    const formData = new FormData(form);
    const value = (key: string) => String(formData.get(key) ?? "").trim();
    if (!value("title") || !value("scope") || !value("done")) {
      setNotice("Enter a title, scope and acceptance criteria.");
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
    showNotice(source === "live" ? `${created.id} sent to hub` : `${created.id} created in this demo`);
  }

  async function handleConnectLive(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const ok = await data.connectLive(tokenInput);
    if (ok) {
      setTokenInput("");
      setMenu(null);
      showNotice("Connected to live Sommelier hub");
    } else {
      showNotice("Hub unreachable or token rejected · staying in demo mode");
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
            <img src={`${import.meta.env.BASE_URL}assets/sommelier-mark.png`} width="30" height="30" alt="" />
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
                <span className="menu-label">{source === "live" ? "Live hub" : "Demo project"}</span>
                <a href="https://github.com/decantlabs/sommelier" target="_blank" rel="noreferrer">
                  Open source repository <ArrowSquareOut size={15} />
                </a>
              </div>
            )}
          </div>
        </div>

        <nav aria-label="Project views">
          {tabs.map((entry) => (
            <button
              key={entry}
              type="button"
              className={tab === entry ? "current" : ""}
              aria-current={tab === entry ? "page" : undefined}
              onClick={() => go(entry)}
            >
              {entry}
            </button>
          ))}
        </nav>

        <div className="header-right">
          <div className="menu-anchor">
            <button
              type="button"
              className="hosting"
              onClick={() => setMenu(menu === "hosting" ? null : "hosting")}
              aria-expanded={menu === "hosting"}
            >
              {hosting === "Self-hosted" ? <Stack size={16} /> : <Cloud size={17} />}
              <span>
                {hosting}
                {hosting === "Self-hosted" ? " · eu-node-01" : ""}
              </span>
              {source === "live" ? (
                <span className="hosting-dot is-live" title="Connected to live hub" />
              ) : data.liveStatus === "auth-required" ? (
                <span className="hosting-dot is-auth" title="Live hub requires a token" />
              ) : null}
            </button>
            {menu === "hosting" && (
              <div className="menu right">
                <span className="menu-label">{source === "live" ? "Connected deployment" : "Demo deployment"}</span>
                {["Self-hosted", "Managed cloud"].map((value) => (
                  <button
                    key={value}
                    type="button"
                    onClick={() => {
                      setHosting(value);
                      setMenu(null);
                    }}
                  >
                    {value}
                    {hosting === value && <Check size={14} />}
                  </button>
                ))}
                <div className="menu-divider" />
                <span className="menu-label">Hub connection ({source})</span>
                {data.liveStatus === "auth-required" && source !== "live" && (
                  <span className="menu-note">Live hub requires a token</span>
                )}
                <form className="menu-form" onSubmit={handleConnectLive}>
                  <label>
                    Hub token (sent via Bearer → HttpOnly session)
                    <input
                      type="password"
                      value={tokenInput}
                      onChange={(e) => setTokenInput(e.target.value)}
                      placeholder="sb_... (empty for public read)"
                    />
                  </label>
                  <Button variant="primary" size="sm" type="submit">
                    {source === "live" ? "Reconnect live hub" : "Connect live hub"}
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
                      Sign out &amp; switch to demo
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
            <h1>Work graph</h1>
            <span>
              Public project <b>·</b>{" "}
              {source === "live" ? `Live hub #${snapshot.hubId}` : "Demo project"}
            </span>
            <div className="view-actions">
              {searchOpen && (
                <input
                  autoFocus
                  aria-label="Search tasks"
                  placeholder="Find a task…"
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
                {searchOpen ? <X size={21} /> : <MagnifyingGlass size={23} />}
              </Button>
              {effectiveRole === "Orchestrator" && (
                <Button
                  variant="primary"
                  onClick={() => {
                    setNotice("");
                    createDialog.current?.showModal();
                  }}
                >
                  <Plus size={18} /> Create task
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
            presenceProviders={
              communityProviders.map((provider) => (
                <span key={provider} title={provider}>
                  <ProviderMark provider={provider} />
                </span>
              ))
            }
            runningAgentCount={runningAgentCount}
          />
        </div>
      ) : null}

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
          {notice && <p role="status">{notice}</p>}
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

      <dialog ref={briefDialog} className="app-dialog">
        <div className="dialog-heading">
          <span className="task-id">{briefTitle}</span>
          <Button variant="icon" aria-label="Close brief" onClick={() => briefDialog.current?.close()}>
            <X size={19} />
          </Button>
        </div>
        <pre className="brief-body">{briefText}</pre>
      </dialog>

      {notice && !createDialog.current?.open && (
        <div className="toast" role="status">
          <Check size={17} /> {notice}
        </div>
      )}

      <span className="sr-only" aria-hidden="true">
        {initialTasks.length}
      </span>
    </main>
  );
}
