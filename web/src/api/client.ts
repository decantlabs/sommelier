import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { adaptApiState, redactSnapshotForPublic } from "./adapter";
import { initialDemoSnapshot } from "./demo";
import type {
  AccessRole,
  ApiStateSnapshot,
  BusMessage,
  DataSourceMode,
  FactorySnapshot,
  HubIdentity,
  SchedulerStatus,
  WorkTask,
} from "./types";

export type CreateTaskInput = {
  title: string;
  scope: string;
  done: string;
  priority: string;
  state: "Draft" | "Ready";
  kind?: string;
  risk?: "low" | "normal" | "guarded";
};

function isLiveApiSnapshot(payload: unknown): payload is ApiStateSnapshot {
  if (!payload || typeof payload !== "object") return false;
  const record = payload as Record<string, unknown>;
  if (record.mode === "demo") return false;
  return "hub" in record || "members" in record || "tasks" in record || "recent" in record;
}

function makeWsUrl(sinceId: number): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  const query = new URLSearchParams();
  query.set("since", String(Math.max(1, sinceId)));
  return `${proto}//${window.location.host}/?${query.toString()}`;
}

function parseRole(raw: unknown): AccessRole {
  if (typeof raw !== "string") return "Observer";
  switch (raw.toLowerCase()) {
    case "orchestrator":
      return "Orchestrator";
    case "member":
      return "Member";
    case "observer":
      return "Observer";
    default:
      return "Public";
  }
}

const REFRESH_KINDS = new Set([
  "TASK_UPDATE",
  "TASK",
  "FACT",
  "ALERT",
  "OFFER",
  "READY",
  "LANDED",
  "CLAIM",
  "RELEASE",
  "POLICY",
  "HELLO",
  "STATUS",
]);

export function useSommelierData(previewRole: AccessRole) {
  const [source, setSource] = useState<DataSourceMode>("demo");
  const [liveStatus, setLiveStatus] = useState<"demo" | "connecting" | "connected" | "auth-required">("demo");
  const [snapshot, setSnapshot] = useState<FactorySnapshot>(initialDemoSnapshot);
  const [identity, setIdentity] = useState<HubIdentity | null>(null);
  const [lastError, setLastError] = useState<string>("");
  const [policyMode, setPolicyMode] = useState<SchedulerStatus["mode"] | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  const hubIdRef = useRef<number>(initialDemoSnapshot.hubId);
  const refreshTimerRef = useRef<number | null>(null);

  useEffect(() => {
    hubIdRef.current = snapshot.hubId;
  }, [snapshot.hubId]);

  // Scrub any `?token=` accidentally pasted into the browser address bar without reading it.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const url = new URL(window.location.href);
    if (url.searchParams.has("token")) {
      url.searchParams.delete("token");
      window.history.replaceState({}, "", `${url.pathname}${url.search}${url.hash}`);
    }
  }, []);

  const loadLivePolicy = useCallback(async () => {
    try {
      const res = await fetch("/api/policy", {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (!res.ok) return;
      const json = (await res.json()) as Record<string, unknown>;
      const pol = json.policy as Record<string, unknown> | undefined;
      const mode = typeof pol?.mode === "string" ? pol.mode.toLowerCase() : "";
      if (mode === "off" || mode === "shadow" || mode === "enforce") {
        setPolicyMode(mode);
      }
    } catch {
      // Policy endpoint optional for public readers.
    }
  }, []);

  const loadSessionIdentity = useCallback(async () => {
    try {
      const res = await fetch("/api/session", {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (!res.ok) {
        if (res.status === 401) {
          setIdentity((prev) => prev ?? { name: "public", role: "Public" });
        }
        return;
      }
      const sess = (await res.json()) as { authenticated?: boolean; name?: string; role?: string };
      if (sess.authenticated && typeof sess.name === "string") {
        setIdentity({ name: sess.name, role: parseRole(sess.role) });
      }
    } catch {
      // Ignore if session probe is unavailable.
    }
  }, []);

  const loadLiveState = useCallback(async (): Promise<boolean> => {
    try {
      const res = await fetch("/api/state", {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (res.status === 401) {
        setLiveStatus("auth-required");
        return false;
      }
      if (!res.ok) return false;
      const ctype = res.headers.get("content-type") ?? "";
      if (!ctype.includes("application/json")) return false;
      const payload: unknown = await res.json();
      if (!isLiveApiSnapshot(payload)) {
        return false;
      }
      const adapted = adaptApiState(payload);
      hubIdRef.current = adapted.hubId;
      setSnapshot(adapted);
      setSource("live");
      setLiveStatus("connected");
      void loadLivePolicy();
      void loadSessionIdentity();
      return true;
    } catch {
      return false;
    }
  }, [loadLivePolicy, loadSessionIdentity]);

  // Probe live hub once on startup. In standalone Vite preview, `/api/state` returns `{ mode: "demo" }`.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const ok = await loadLiveState();
      if (cancelled) return;
      if (!ok) {
        setLiveStatus((prev) => (prev === "auth-required" ? "auth-required" : "demo"));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [loadLiveState]);

  // Live WebSocket stream with exponential backoff reconnect and `since` cursor resume.
  // Authentication uses the same-origin `HttpOnly` session cookie (never query parameters).
  useEffect(() => {
    if (source !== "live") return;
    let closed = false;
    let attempt = 0;
    let reconnectTimer: number | null = null;
    const delays = [1000, 2000, 4000, 8000, 15000];

    function connect() {
      if (closed) return;
      const ws = new WebSocket(makeWsUrl(hubIdRef.current + 1));
      wsRef.current = ws;

      ws.onopen = () => {
        attempt = 0;
        ws.send(JSON.stringify({ op: "whoami", rid: "ui-whoami" }));
      };

      ws.onmessage = (event) => {
        if (closed || typeof event.data !== "string") return;
        try {
          const frame = JSON.parse(event.data) as Record<string, unknown>;
          if (typeof frame.reply === "string") {
            if (frame.reply === "whoami" && frame.ok === true) {
              setIdentity({
                name: typeof frame.name === "string" ? frame.name : "public",
                role: parseRole(frame.role),
                tier: typeof frame.tier === "number" ? frame.tier : undefined,
              });
            } else if (frame.ok === false && typeof frame.error === "string") {
              setLastError(frame.error);
            }
            return;
          }

          if (typeof frame.id === "number" && typeof frame.kind === "string") {
            const msg: BusMessage = {
              id: frame.id,
              at: typeof frame.ts === "string" && frame.ts.includes("T") ? frame.ts.slice(11, 19) : "now",
              from: typeof frame.from === "string" ? frame.from : "hub",
              role:
                frame.role === "hub"
                  ? "Hub"
                  : frame.role === "orchestrator"
                    ? "Orchestrator"
                    : "Member",
              kind: frame.kind,
              topic: typeof frame.topic === "string" ? frame.topic : "-",
              body: typeof frame.body === "string" ? frame.body : "",
              refs: Array.isArray(frame.refs)
                ? frame.refs.filter((r): r is string => typeof r === "string")
                : [],
              to: Array.isArray(frame.to)
                ? frame.to.filter((r): r is string => typeof r === "string")
                : undefined,
            };
            hubIdRef.current = Math.max(hubIdRef.current, msg.id);
            setSnapshot((prev) => ({
              ...prev,
              hubId: Math.max(prev.hubId, msg.id),
              messages: [msg, ...prev.messages.filter((item) => item.id !== msg.id)],
            }));
            if (REFRESH_KINDS.has(msg.kind)) {
              if (refreshTimerRef.current) window.clearTimeout(refreshTimerRef.current);
              refreshTimerRef.current = window.setTimeout(() => {
                void loadLiveState();
              }, 250);
            }
          }
        } catch {
          // Ignore malformed JSON frames cleanly.
        }
      };

      ws.onclose = () => {
        wsRef.current = null;
        if (closed) return;
        const wait = delays[Math.min(attempt, delays.length - 1)];
        attempt += 1;
        reconnectTimer = window.setTimeout(connect, wait);
      };
    }

    connect();

    return () => {
      closed = true;
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      if (refreshTimerRef.current) window.clearTimeout(refreshTimerRef.current);
      wsRef.current?.close();
      wsRef.current = null;
    };
  }, [source, loadLiveState]);

  const connectLive = useCallback(
    async (nextToken: string): Promise<boolean> => {
      const trimmed = nextToken.trim();
      setLastError("");
      setLiveStatus("connecting");
      if (trimmed) {
        try {
          const res = await fetch("/api/session", {
            method: "POST",
            headers: {
              Accept: "application/json",
              Authorization: `Bearer ${trimmed}`,
            },
            credentials: "same-origin",
            cache: "no-store",
          });
          if (!res.ok) {
            let msg = "Token rejected by hub";
            try {
              const errJson = (await res.json()) as { error?: string };
              if (errJson.error) msg = errJson.error;
            } catch {
              // Keep generic message.
            }
            setLastError(msg);
            setSource("demo");
            setLiveStatus("auth-required");
            setIdentity(null);
            return false;
          }
          const sess = (await res.json()) as { name?: string; role?: string };
          setIdentity({
            name: typeof sess.name === "string" ? sess.name : "member",
            role: parseRole(sess.role),
          });
        } catch {
          setSource("demo");
          setLiveStatus("demo");
          setIdentity(null);
          return false;
        }
      } else {
        try {
          await fetch("/api/session", {
            method: "DELETE",
            credentials: "same-origin",
            cache: "no-store",
          });
        } catch {
          // Standalone Vite preview has no /api/session endpoint.
        }
      }

      const ok = await loadLiveState();
      if (!ok) {
        setSource("demo");
        setIdentity(null);
        return false;
      }
      if (!trimmed) {
        setIdentity({ name: "public", role: "Public" });
      }
      return true;
    },
    [loadLiveState],
  );

  const switchToDemo = useCallback(async () => {
    wsRef.current?.close();
    wsRef.current = null;
    if (source === "live") {
      try {
        await fetch("/api/session", {
          method: "DELETE",
          credentials: "same-origin",
          cache: "no-store",
        });
      } catch {
        // Ignore network errors while signing out.
      }
    }
    setSource("demo");
    setLiveStatus("demo");
    setIdentity(null);
    setPolicyMode(null);
    setLastError("");
    hubIdRef.current = initialDemoSnapshot.hubId;
    setSnapshot(initialDemoSnapshot);
  }, [source]);

  const effectiveRole: AccessRole = source === "live" && identity ? identity.role : previewRole;

  const createTask = useCallback(
    (input: CreateTaskInput): WorkTask | null => {
      if (effectiveRole !== "Orchestrator") return null;
      const customCount = snapshot.tasks.length - initialDemoSnapshot.tasks.length;
      const id = `SOM-${146 + Math.max(0, customCount)}`;
      const newTask: WorkTask = {
        id,
        title: input.title,
        scope: input.scope,
        done: input.done,
        priority: input.priority,
        state: input.state,
        kind: input.kind ?? "fix",
        risk: input.risk ?? "normal",
        minTrust: input.risk === "guarded" ? 2 : 1,
        attempt: 1,
        maxAttempts: 3,
        x: 105 + (Math.max(0, customCount) % 3) * 450,
        y: 640 + Math.floor(Math.max(0, customCount) / 3) * 190,
      };
      setSnapshot((prev) => ({
        ...prev,
        tasks: [...prev.tasks, newTask],
      }));
      if (source === "live" && wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(
          JSON.stringify({
            op: "task.add",
            id: newTask.id,
            title: newTask.title,
            kind: newTask.kind ?? "fix",
            scope: newTask.scope,
            done_when: newTask.done,
            priority: newTask.priority?.toLowerCase() ?? "normal",
            state: newTask.state.toLowerCase(),
          }),
        );
      }
      return newTask;
    },
    [effectiveRole, snapshot.tasks.length, source],
  );

  const postMessage = useCallback(
    (topic: string, body: string, recipient: string): BusMessage | null => {
      if (effectiveRole === "Observer" || effectiveRole === "Public") return null;
      const senderName = identity?.name || "mira";
      const newMessage: BusMessage = {
        id: (snapshot.messages[0]?.id ?? snapshot.hubId) + 1,
        at: new Date().toLocaleTimeString("en-GB", { hour12: false }),
        from: senderName,
        role: effectiveRole,
        kind: "CHAT",
        topic,
        body,
        refs: topic.split("/").filter((part) => /^SOM-\d+$/.test(part)),
        to: recipient ? [recipient] : [],
      };
      setSnapshot((prev) => ({
        ...prev,
        hubId: Math.max(prev.hubId, newMessage.id),
        messages: [newMessage, ...prev.messages],
      }));
      if (source === "live" && wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(
          JSON.stringify({
            op: "send",
            kind: "CHAT",
            topic: newMessage.topic,
            body: newMessage.body,
            refs: newMessage.refs,
            to: newMessage.to,
          }),
        );
      }
      return newMessage;
    },
    [effectiveRole, identity?.name, snapshot.hubId, snapshot.messages, source],
  );

  const ackAlert = useCallback(
    (key: string) => {
      if (effectiveRole !== "Orchestrator") return;
      setSnapshot((prev) => ({
        ...prev,
        alerts: prev.alerts.map((a) => (a.key === key ? { ...a, acked: true } : a)),
      }));
      if (source === "live" && wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ op: "alert.ack", key }));
      }
    },
    [effectiveRole, source],
  );

  const togglePauseAll = useCallback(() => {
    if (effectiveRole !== "Orchestrator") return;
    const isPaused = snapshot.scheduler.activePauses.includes("all");
    setSnapshot((prev) => ({
      ...prev,
      scheduler: {
        ...prev.scheduler,
        activePauses: isPaused
          ? prev.scheduler.activePauses.filter((p) => p !== "all")
          : ["all", ...prev.scheduler.activePauses],
      },
    }));
    if (source === "live" && wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({ op: isPaused ? "plan.resume" : "plan.pause", scope: "all" }));
    }
  }, [effectiveRole, snapshot.scheduler.activePauses, source]);

  const fetchBrief = useCallback(
    async (taskId: string): Promise<string> => {
      if (source === "live") {
        try {
          const res = await fetch(`/api/brief/${encodeURIComponent(taskId)}`, {
            credentials: "same-origin",
            cache: "no-store",
          });
          if (res.ok) return await res.text();
        } catch {
          // Fall through to cached/demo brief.
        }
      }
      return snapshot.briefs[taskId] ?? `# ${taskId}\n\nNo extended brief stored for this task.`;
    },
    [snapshot.briefs, source],
  );

  const visibleSnapshot = useMemo(() => {
    const base = policyMode
      ? { ...snapshot, scheduler: { ...snapshot.scheduler, mode: policyMode } }
      : snapshot;
    return effectiveRole === "Public" ? redactSnapshotForPublic(base) : base;
  }, [effectiveRole, policyMode, snapshot]);

  return {
    snapshot: visibleSnapshot,
    source,
    liveStatus,
    identity,
    effectiveRole,
    lastError,
    clearError: () => setLastError(""),
    connectLive,
    switchToDemo,
    refreshLive: () => loadLiveState(),
    createTask,
    postMessage,
    ackAlert,
    togglePauseAll,
    fetchBrief,
  };
}
