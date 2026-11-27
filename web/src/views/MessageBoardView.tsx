import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import {
  ArrowDown,
  ArrowRight,
  Broadcast,
  ChatCircle,
  Check,
  Code,
  Copy,
  Crosshair,
  EyeSlash,
  Hash,
  PaperPlaneRight,
  Pulse,
  Tray,
} from "@phosphor-icons/react";
import { isAckedOrClosedAlert, parseMessageBody } from "../api/format";
import type { AccessRole, BusMessage, DataSourceMode } from "../api/types";
import {
  Button,
  DefinitionList,
  FilterBar,
  FilterField,
  FilterToggle,
  InspectorPanel,
  SegmentedControl,
  messageKindTone,
  StatusLabel,
} from "../components";
import "./message-board.css";

type Props = {
  messages: BusMessage[];
  currentUser: string;
  recipients: string[];
  role: AccessRole;
  source: DataSourceMode;
  topic: string;
  setTopic: (topic: string) => void;
  onTask: (id: string) => void;
  onPublish: (topic: string, body: string, recipient: string) => BusMessage | null;
};

const messageLimit = 600;
const taskReference = /^(?:SOM|T)-\d+$/;
const messagePageSize = 25;
type Stream = "all" | "chat" | "findings" | "events";

function matchesTopic(value: string, filter: string) {
  if (!filter) return true;
  if (filter.endsWith("*")) return value.startsWith(filter.slice(0, -1));
  return value === filter || value.toLowerCase().includes(filter.toLowerCase());
}

function isAddressedTo(message: BusMessage, user: string) {
  return message.to?.some((recipient) => recipient.split("/")[0] === user) ?? false;
}

export default function MessageBoardView({
  messages,
  currentUser,
  recipients,
  role,
  source,
  topic,
  setTopic,
  onTask,
  onPublish,
}: Props) {
  const [selected, setSelected] = useState<number | null>(null);
  const [kind, setKind] = useState("");
  const [sender, setSender] = useState("");
  const [forMe, setForMe] = useState(false);
  const [hideAcked, setHideAcked] = useState(true);
  const [draft, setDraft] = useState("");
  const [draftTopic, setDraftTopic] = useState<string | null>(null);
  const [recipient, setRecipient] = useState("");
  const [posted, setPosted] = useState(false);
  const [copiedRaw, setCopiedRaw] = useState(false);
  const [stream, setStream] = useState<Stream>("all");
  const [search, setSearch] = useState("");
  const [page, setPage] = useState(0);
  const [isComposing, setComposing] = useState(false);
  const composer = useRef<HTMLInputElement>(null);
  const canPost = role === "Member" || role === "Orchestrator";

  useEffect(() => {
    if (canPost && isComposing) composer.current?.focus();
  }, [canPost, isComposing]);

  const parsedMap = useMemo(() => {
    const map = new Map<number, ReturnType<typeof parseMessageBody>>();
    for (const msg of messages) {
      map.set(msg.id, parseMessageBody(msg));
    }
    return map;
  }, [messages]);

  const ackedAlertCount = useMemo(
    () => messages.filter((m) => isAckedOrClosedAlert(m)).length,
    [messages],
  );
  const chatCount = useMemo(
    () => messages.filter((m) => m.kind === "CHAT").length,
    [messages],
  );
  const findingCount = useMemo(
    () => messages.filter((m) => m.kind === "FINDING").length,
    [messages],
  );
  const eventCount = useMemo(
    () => messages.filter((m) => m.kind !== "CHAT" && m.kind !== "FINDING").length,
    [messages],
  );

  const kinds = useMemo(() => [...new Set(messages.map((message) => message.kind))].sort(), [messages]);
  const senders = useMemo(() => [...new Set(messages.map((message) => message.from))].sort(), [messages]);
  const topicSuggestions = useMemo(() => {
    const prefixes = new Set<string>();
    for (const m of messages) {
      if (!m.topic || m.topic === "-") continue;
      prefixes.add(m.topic);
      const firstSeg = m.topic.split("/")[0];
      if (firstSeg && m.topic.includes("/")) prefixes.add(`${firstSeg}/*`);
    }
    return [...prefixes].slice(0, 20);
  }, [messages]);

  const query = search.trim().toLowerCase();
  const visible = useMemo(
    () =>
      messages.filter((message) => {
        const parsed = parsedMap.get(message.id);
        const matchesStream =
          stream === "all" ||
          (stream === "chat" && message.kind === "CHAT") ||
          (stream === "findings" && message.kind === "FINDING") ||
          (stream === "events" && message.kind !== "CHAT" && message.kind !== "FINDING");
        const matchesAcked = !hideAcked || kind === "ALERT" || !isAckedOrClosedAlert(message);
        const matchesQuery =
          !query ||
          [message.body, parsed?.title ?? "", message.from, message.topic, ...message.refs].some((value) =>
            value.toLowerCase().includes(query),
          );
        return (
          matchesStream &&
          matchesAcked &&
          matchesQuery &&
          (!kind || message.kind === kind) &&
          (!sender || message.from === sender) &&
          matchesTopic(message.topic, topic.trim()) &&
          (!forMe || !message.to?.length || isAddressedTo(message, currentUser))
        );
      }),
    [currentUser, forMe, hideAcked, kind, messages, parsedMap, query, sender, stream, topic],
  );

  useEffect(() => {
    if (selected !== null && visible.some((m) => m.id === selected)) return;
    setSelected(visible[0]?.id ?? null);
  }, [selected, visible]);

  useEffect(() => {
    setCopiedRaw(false);
  }, [selected]);

  const active = visible.find((message) => message.id === selected);
  const activeParsed = active ? parsedMap.get(active.id) : undefined;
  const pageCount = Math.max(1, Math.ceil(visible.length / messagePageSize));
  const currentPage = Math.min(page, pageCount - 1);
  const pageMessages = visible.slice(currentPage * messagePageSize, (currentPage + 1) * messagePageSize);
  const filtered = Boolean(kind || sender || topic || forMe || query || stream !== "all");
  const effectiveTopic = draftTopic ?? (topic && !topic.endsWith("*") ? topic : "chat/general");

  function resetFilters() {
    setKind("");
    setSender("");
    setTopic("");
    setForMe(false);
    setStream("all");
    setSearch("");
    setPage(0);
  }

  function publish(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const body = draft.trim();
    const messageTopic = effectiveTopic.trim();
    if (
      role === "Observer" ||
      role === "Public" ||
      !body ||
      Array.from(body).length > messageLimit ||
      !messageTopic ||
      /\s|\*/.test(messageTopic)
    ) {
      return;
    }
    const created = onPublish(messageTopic, body, recipient);
    if (!created) return;
    setDraft("");
    setSelected(created.id);
    setPosted(true);
    setComposing(false);
    resetFilters();
  }

  async function copyRawJson(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedRaw(true);
      window.setTimeout(() => setCopiedRaw(false), 2000);
    } catch {
      // Ignore clipboard failure silently.
    }
  }

  return (
    <section className="message-board" aria-label="Message board">
      <div className="mb-header">
        <div className="mb-header-title">
          <div className="mb-header-icon">
            <Pulse size={20} />
          </div>
          <div>
            <h1>Message board</h1>
            <p>Read project events, findings, and agent conversations by topic.</p>
          </div>
        </div>
        <div className="mb-toolbar-actions">
          <span className="mb-log-label">
            <span /> {source === "live" ? "Live stream" : "Demo log"}
          </span>
          {canPost && (
            <Button
              size="sm"
              variant="primary"
              aria-expanded={isComposing}
              aria-controls={isComposing ? "mb-composer" : undefined}
              onClick={() => {
                setPosted(false);
                setComposing(!isComposing);
              }}
            >
              <ChatCircle size={16} /> {isComposing ? "Close composer" : "Write a message"}
            </Button>
          )}
        </div>
      </div>

      <div className="mb-stream-bar">
        <SegmentedControl
          size="sm"
          ariaLabel="Message streams"
          value={stream}
          options={[
            { value: "all", label: `All (${hideAcked ? messages.length - ackedAlertCount : messages.length})` },
            { value: "chat", label: `Chat (${chatCount})` },
            ...(findingCount > 0 ? [{ value: "findings" as const, label: `Findings (${findingCount})` }] : []),
            { value: "events", label: `Bus events (${eventCount})` },
          ]}
          onChange={(next) => {
            setKind("");
            setPage(0);
            setStream(next);
          }}
        />
        <div className="mb-stream-meta">
          {ackedAlertCount > 0 && (
            <FilterToggle
              pressed={hideAcked}
              onClick={() => {
                setHideAcked(!hideAcked);
                setPage(0);
              }}
              icon={<EyeSlash size={14} />}
            >
              Hide acked alerts ({ackedAlertCount})
            </FilterToggle>
          )}
          {posted && (
            <span className="mb-posted" role="status">
              {source === "live" ? "Message sent." : "Added to demo log."}
            </span>
          )}
        </div>
      </div>

      <FilterBar ariaLabel="Message filters">
        <FilterField label="Search">
          <input
            value={search}
            onChange={(event) => {
              setSearch(event.target.value);
              setPage(0);
            }}
            placeholder="Message, topic, or ref"
          />
        </FilterField>
        <FilterField label="Kind">
          <select
            value={kind}
            onChange={(event) => {
              setKind(event.target.value);
              setPage(0);
            }}
          >
            <option value="">All kinds</option>
            {kinds.map((item) => (
              <option key={item} value={item}>
                {item.replaceAll("_", " ")}
              </option>
            ))}
          </select>
        </FilterField>
        <FilterField label="Topic">
          <input
            className="mb-topic-input"
            list="mb-topic-suggestions"
            value={topic}
            onChange={(event) => {
              setTopic(event.target.value);
              setPage(0);
            }}
            placeholder="e.g. side/*, alert/*"
          />
          <datalist id="mb-topic-suggestions">
            {topicSuggestions.map((t) => (
              <option key={t} value={t} />
            ))}
          </datalist>
        </FilterField>
        <FilterField label="Sender">
          <select
            value={sender}
            onChange={(event) => {
              setSender(event.target.value);
              setPage(0);
            }}
          >
            <option value="">Anyone</option>
            {senders.map((item) => (
              <option key={item} value={item}>
                {item}
              </option>
            ))}
          </select>
        </FilterField>
        <FilterToggle
          pressed={forMe}
          onClick={() => {
            setForMe(!forMe);
            setPage(0);
          }}
          icon={<Crosshair size={15} />}
        >
          For me
        </FilterToggle>
        {filtered && (
          <Button variant="ghost" size="sm" onClick={resetFilters}>
            Clear filters
          </Button>
        )}
      </FilterBar>

      {canPost && isComposing ? (
        <form className="mb-compose" id="mb-composer" onSubmit={publish}>
          <div className="mb-compose-title">
            <ChatCircle size={17} />
            <strong>Post a message</strong>
            <span>{source === "live" ? "Live bus" : "Local demo"}</span>
          </div>
          <div className="mb-compose-fields">
            <label className="mb-compose-topic">
              <span>Topic</span>
              <input
                value={effectiveTopic}
                onChange={(event) => setDraftTopic(event.target.value)}
                maxLength={128}
                pattern="[^\s*]+"
                required
              />
            </label>
            <label>
              <span>To</span>
              <select value={recipient} onChange={(event) => setRecipient(event.target.value)}>
                <option value="">Everyone</option>
                {recipients.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <label className="mb-compose-body">
              <span>Message</span>
              <input
                ref={composer}
                value={draft}
                onChange={(event) => {
                  setPosted(false);
                  setDraft(event.target.value);
                }}
                placeholder="Write a short update…"
                maxLength={messageLimit}
                required
              />
            </label>
            <Button variant="primary" size="sm" type="submit" className="mb-send" disabled={!draft.trim()}>
              <PaperPlaneRight size={16} /> Post
            </Button>
          </div>
          <div className="mb-compose-footer">
            <span role="status">
              {posted
                ? source === "live"
                  ? "Message sent to hub."
                  : "Message added to the demo log."
                : ""}
            </span>
            <span>
              {Array.from(draft).length}/{messageLimit}
            </span>
          </div>
        </form>
      ) : canPost ? null : (
        <div className="mb-readonly">
          <Tray size={15} /> {role} access · Read-only stream
        </div>
      )}

      <div className="mb-content" data-detail={Boolean(active)}>
        <div className="mb-log">
          <div className="mb-log-header">
            <span>
              Messages <span className="mb-count">{visible.length}</span>
            </span>
            <span>
              <ArrowDown size={13} /> Newest first
            </span>
          </div>
          {visible.length === 0 ? (
            <div className="mb-empty">
              <Tray size={26} />
              <strong>No matching messages</strong>
              <p>
                {hideAcked && ackedAlertCount > 0
                  ? `${ackedAlertCount} acknowledged alerts are hidden. Adjust filters or show acked alerts.`
                  : "Adjust the filters to see more of the log."}
              </p>
            </div>
          ) : (
            <ol className="mb-rows">
              {pageMessages.map((message) => {
                const parsed = parsedMap.get(message.id) ?? parseMessageBody(message);
                const isAcked = parsed.alertState === "acked" || parsed.alertState === "closed";
                return (
                  <li key={message.id}>
                    <button
                      type="button"
                      className={`mb-row ${isAcked ? "is-muted" : ""}`}
                      aria-pressed={active?.id === message.id}
                      onClick={() => setSelected(message.id)}
                    >
                      <time>{message.at}</time>
                      <StatusLabel
                        variant="chip"
                        tone={isAcked ? "muted" : messageKindTone(message.kind)}
                      >
                        {parsed.alertCode ?? message.kind.replaceAll("_", " ")}
                      </StatusLabel>
                      <span className="mb-row-main">
                        <span className={`mb-row-body ${message.kind === "CHAT" || message.kind === "FINDING" ? "is-multiline" : ""}`}>
                          {parsed.title}
                        </span>
                        <span className="mb-row-meta">
                          <code>{message.topic}</code>
                          <span>·</span>
                          <span>{message.from}</span>
                          {parsed.alertState && (
                            <>
                              <span>·</span>
                              <span className={`mb-state-tag is-${parsed.alertState}`}>
                                {parsed.alertState}
                              </span>
                            </>
                          )}
                          {parsed.count && parsed.count > 1 && (
                            <span className="mb-repeat-tag mono">×{parsed.count}</span>
                          )}
                        </span>
                      </span>
                      <span
                        className="mb-row-audience"
                        title={message.to?.length ? `To ${message.to.join(", ")}` : "Broadcast"}
                      >
                        {message.to?.length ? <Crosshair size={15} /> : <Broadcast size={15} />}
                        <span className="sr-only">
                          {message.to?.length ? `To ${message.to.join(", ")}` : "Broadcast"}
                        </span>
                      </span>
                    </button>
                  </li>
                );
              })}
            </ol>
          )}
          {pageCount > 1 && (
            <div className="mb-pagination">
              <span>
                {currentPage * messagePageSize + 1}–{Math.min((currentPage + 1) * messagePageSize, visible.length)} of{" "}
                {visible.length}
              </span>
              <div>
                <Button size="sm" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>
                  Previous
                </Button>
                <Button size="sm" disabled={currentPage + 1 >= pageCount} onClick={() => setPage(currentPage + 1)}>
                  Next
                </Button>
              </div>
            </div>
          )}
        </div>

        {active && activeParsed && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Message detail"
            eyebrow={
              <>
                <Hash size={15} />
                <span>Message {active.id}</span>
              </>
            }
            title={
              activeParsed.isStructured
                ? activeParsed.title
                : active.kind === "CHAT" || active.kind === "FINDING"
                  ? `${active.from} on ${active.topic}`
                  : activeParsed.title
            }
            onClose={() => setSelected(null)}
            closeLabel="Close message detail"
          >
            <div className="mb-detail-badges">
              <StatusLabel
                variant="pill"
                tone={
                  activeParsed.alertState === "acked" || activeParsed.alertState === "closed"
                    ? "muted"
                    : messageKindTone(active.kind)
                }
              >
                {active.kind.replaceAll("_", " ")}
              </StatusLabel>
              {activeParsed.alertCode && (
                <StatusLabel
                  variant="pill"
                  tone={activeParsed.severity === "error" ? "danger" : activeParsed.alertState === "open" ? "warning" : "muted"}
                >
                  {activeParsed.alertCode}
                  {activeParsed.alertState ? ` · ${activeParsed.alertState}` : ""}
                </StatusLabel>
              )}
            </div>

            {!activeParsed.isStructured && (
              <p className="mb-detail-body">{activeParsed.title}</p>
            )}

            {activeParsed.isStructured && activeParsed.fields.length > 0 && (
              <div className="mb-structured-card">
                <DefinitionList
                  items={activeParsed.fields.map((field) => ({
                    label: field.label,
                    value: field.mono ? <span className="mono">{field.value}</span> : field.value,
                  }))}
                />
              </div>
            )}

            <DefinitionList
              items={[
                {
                  label: "Topic",
                  value: (
                    <Button
                      variant="link"
                      onClick={() => {
                        setTopic(active.topic);
                        setPage(0);
                      }}
                    >
                      <code>{active.topic}</code> <ArrowRight size={12} />
                    </Button>
                  ),
                },
                {
                  label: "Sender",
                  value: (
                    <>
                      {active.from} <span className="mb-muted">· {active.role}</span>
                    </>
                  ),
                },
                {
                  label: "Audience",
                  value: (
                    <span className="mb-audience">
                      {active.to?.length ? <Crosshair size={14} /> : <Broadcast size={14} />}
                      {active.to?.length ? active.to.join(", ") : "Broadcast"}
                    </span>
                  ),
                },
                { label: "Time", value: <span className="mb-mono">{active.at}</span> },
              ]}
            />

            {active.refs.length > 0 && (
              <div className="mb-refs">
                <h3>References</h3>
                <div>
                  {active.refs.map((ref) =>
                    taskReference.test(ref) ? (
                      <button
                        type="button"
                        key={ref}
                        className="mb-ref mb-task-ref"
                        onClick={() => onTask(ref)}
                      >
                        {ref}
                        <ArrowRight size={12} />
                      </button>
                    ) : (
                      <code key={ref} className="mb-ref">
                        {ref}
                      </code>
                    ),
                  )}
                </div>
              </div>
            )}

            {activeParsed.rawPretty && (
              <details className="mb-raw-disclosure">
                <summary>
                  <Code size={14} /> Raw event payload
                </summary>
                <div className="mb-raw-actions">
                  <Button size="sm" variant="ghost" onClick={() => copyRawJson(activeParsed.rawPretty!)}>
                    {copiedRaw ? <Check size={13} /> : <Copy size={13} />}
                    {copiedRaw ? "Copied" : "Copy JSON"}
                  </Button>
                </div>
                <pre className="mb-raw-json">{activeParsed.rawPretty}</pre>
              </details>
            )}
          </InspectorPanel>
        )}
      </div>
    </section>
  );
}
