import { useEffect, useMemo, useState, type FormEvent } from "react";
import {
  ArrowDown,
  ArrowRight,
  Broadcast,
  ChatCircle,
  Crosshair,
  Hash,
  PaperPlaneRight,
  Pulse,
  Tray,
} from "@phosphor-icons/react";
import type { AccessRole, BusMessage, DataSourceMode } from "../api/types";
import {
  Button,
  DefinitionList,
  FilterBar,
  FilterField,
  FilterToggle,
  InspectorPanel,
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

function matchesTopic(value: string, filter: string) {
  if (!filter) return true;
  return filter.endsWith("*") ? value.startsWith(filter.slice(0, -1)) : value === filter;
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
  const [selected, setSelected] = useState<number | null>(messages[0]?.id ?? null);
  const [kind, setKind] = useState("");
  const [sender, setSender] = useState("");
  const [forMe, setForMe] = useState(false);
  const [draft, setDraft] = useState("");
  const [draftTopic, setDraftTopic] = useState<string | null>(null);
  const [recipient, setRecipient] = useState("");
  const [posted, setPosted] = useState(false);

  useEffect(() => {
    if (selected !== null && messages.some((m) => m.id === selected)) return;
    setSelected(messages[0]?.id ?? null);
  }, [messages, selected]);

  const kinds = useMemo(() => [...new Set(messages.map((message) => message.kind))].sort(), [messages]);
  const senders = useMemo(() => [...new Set(messages.map((message) => message.from))].sort(), [messages]);
  const visible = messages.filter(
    (message) =>
      (!kind || message.kind === kind) &&
      (!sender || message.from === sender) &&
      matchesTopic(message.topic, topic.trim()) &&
      (!forMe || !message.to?.length || isAddressedTo(message, currentUser)),
  );
  const active = visible.find((message) => message.id === selected);
  const filtered = Boolean(kind || sender || topic || forMe);
  const effectiveTopic = draftTopic ?? (topic && !topic.endsWith("*") ? topic : "chat/general");

  function resetFilters() {
    setKind("");
    setSender("");
    setTopic("");
    setForMe(false);
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
    resetFilters();
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
            <p>Project events and participant messages</p>
          </div>
        </div>
        <span className="mb-log-label">
          <span /> {source === "live" ? "Live stream" : "Demo log"}
        </span>
      </div>

      <FilterBar ariaLabel="Message filters">
        <FilterField label="Kind">
          <select value={kind} onChange={(event) => setKind(event.target.value)}>
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
            value={topic}
            onChange={(event) => setTopic(event.target.value)}
            placeholder="task/*"
          />
        </FilterField>
        <FilterField label="Sender">
          <select value={sender} onChange={(event) => setSender(event.target.value)}>
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
          onClick={() => setForMe(!forMe)}
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
              <p>Adjust the filters to see more of the log.</p>
            </div>
          ) : (
            <ol className="mb-rows">
              {visible.map((message) => (
                <li key={message.id}>
                  <button
                    type="button"
                    className="mb-row"
                    aria-pressed={active?.id === message.id}
                    onClick={() => setSelected(message.id)}
                  >
                    <time>{message.at}</time>
                    <StatusLabel variant="chip" tone={messageKindTone(message.kind)}>
                      {message.kind.replaceAll("_", " ")}
                    </StatusLabel>
                    <span className="mb-row-main">
                      <span className="mb-row-body">{message.body}</span>
                      <span className="mb-row-meta">
                        <code>{message.topic}</code>
                        <span>·</span>
                        {message.from}
                      </span>
                    </span>
                    <span
                      className="mb-row-audience"
                      title={message.to?.length ? `To ${message.to.join(", ")}` : "Broadcast"}
                    >
                      {message.to?.length ? <Crosshair size={16} /> : <Broadcast size={16} />}
                      <span className="sr-only">
                        {message.to?.length ? `To ${message.to.join(", ")}` : "Broadcast"}
                      </span>
                    </span>
                  </button>
                </li>
              ))}
            </ol>
          )}
        </div>

        {active && (
          <InspectorPanel
            mode="docked"
            ariaLabel="Message detail"
            eyebrow={
              <>
                <Hash size={15} />
                <span>Message {active.id}</span>
              </>
            }
            title={active.body}
            onClose={() => setSelected(null)}
            closeLabel="Close message detail"
          >
            <div style={{ marginBottom: 10 }}>
              <StatusLabel variant="chip" tone={messageKindTone(active.kind)}>
                {active.kind.replaceAll("_", " ")}
              </StatusLabel>
            </div>
            <DefinitionList
              items={[
                { label: "Topic", value: <span className="mb-mono">{active.topic}</span> },
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
          </InspectorPanel>
        )}
      </div>

      {role === "Observer" || role === "Public" ? (
        <div className="mb-readonly">
          <Tray size={16} /> {role} access · Read only
        </div>
      ) : (
        <form className="mb-compose" onSubmit={publish}>
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
      )}
    </section>
  );
}
