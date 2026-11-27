import type { BusMessage, WorkTask } from "./types";

export type ParsedField = {
  label: string;
  value: string;
  mono?: boolean;
};

export type ParsedMessage = {
  isStructured: boolean;
  title: string;
  subtitle?: string;
  alertCode?: string;
  alertState?: string;
  severity?: "info" | "warn" | "error";
  count?: number;
  fields: ParsedField[];
  rawPretty?: string;
};

/** Formats a duration in seconds into a concise human-readable string (e.g. "11h 4m", "10m", "45s"). */
export function formatDuration(totalSeconds: number): string {
  if (!Number.isFinite(totalSeconds) || totalSeconds < 0) return "0s";
  const sec = Math.floor(totalSeconds);
  if (sec < 60) return `${sec}s`;
  const mins = Math.floor(sec / 60);
  if (mins < 60) {
    const remSec = sec % 60;
    return remSec > 0 && mins < 5 ? `${mins}m ${remSec}s` : `${mins}m`;
  }
  const hours = Math.floor(mins / 60);
  const remMin = mins % 60;
  if (hours < 24) {
    return remMin > 0 ? `${hours}h ${remMin}m` : `${hours}h`;
  }
  const days = Math.floor(hours / 24);
  const remHours = hours % 24;
  return remHours > 0 ? `${days}d ${remHours}h` : `${days}d`;
}

/** Replaces raw second counts like "39861 s" or "600s" in alert text with human-readable durations. */
export function humanizeTextDurations(text: string): string {
  if (!text) return text;
  return text.replace(/\b(\d{2,})\s*s\b/g, (_match, rawSecs: string) => {
    const n = Number(rawSecs);
    if (!Number.isFinite(n) || n < 60) return `${rawSecs}s`;
    return formatDuration(n);
  });
}

/** Formats a Unix timestamp (seconds or milliseconds) into a short readable date/time. */
export function formatEpochTimestamp(raw: number): string {
  if (!Number.isFinite(raw) || raw <= 0) return "—";
  const ms = raw > 1e12 ? raw : raw * 1000;
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return String(raw);
  const month = date.toLocaleString("en-US", { month: "short" });
  const day = date.getDate();
  const time = date.toLocaleTimeString("en-GB", { hour12: false });
  return `${month} ${day}, ${time}`;
}

/** Formats a batch identifier such as `batch-1795638919` into a compact readable label. */
export function formatBatchLabel(batchId: string): { label: string; timestamp?: string } {
  const match = /^batch-(\d{9,13})$/.exec(batchId.trim());
  if (!match) {
    return { label: batchId };
  }
  const raw = Number(match[1]);
  const ms = raw > 1e12 ? raw : raw * 1000;
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) {
    return { label: batchId };
  }
  const time = date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false });
  return {
    label: `Batch ${time}`,
    timestamp: formatEpochTimestamp(raw),
  };
}

export function pluralize(count: number, singular: string, plural?: string): string {
  return `${count} ${count === 1 ? singular : (plural ?? `${singular}s`)}`;
}

function asObj(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

/** Parses bus message bodies, turning raw JSON payloads (e.g. ALERT/FACT envelopes) into clean readable fields. */
export function parseMessageBody(message: Pick<BusMessage, "kind" | "topic" | "body">): ParsedMessage {
  const trimmed = message.body.trim();
  if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      const obj = asObj(parsed);
      if (obj) {
        const code = typeof obj.code === "string" ? obj.code : undefined;
        const state = typeof obj.state === "string" ? obj.state : undefined;
        const severityRaw = typeof obj.severity === "string" ? obj.severity.toLowerCase() : undefined;
        const severity: ParsedMessage["severity"] =
          severityRaw === "error" || severityRaw === "crit" || severityRaw === "critical"
            ? "error"
            : severityRaw === "warn" || severityRaw === "warning"
              ? "warn"
              : severityRaw === "info"
                ? "info"
                : undefined;
        const count = typeof obj.count === "number" && Number.isFinite(obj.count) ? obj.count : undefined;
        const rawText = typeof obj.text === "string" ? obj.text : typeof obj.message === "string" ? obj.message : undefined;
        const key = typeof obj.key === "string" ? obj.key : undefined;
        const data = asObj(obj.data);

        const title = rawText
          ? humanizeTextDurations(rawText.replace(/^slot\s+/i, ""))
          : code && key
            ? `${code} · ${key}`
            : code || key || message.topic;

        const fields: ParsedField[] = [];
        if (code) fields.push({ label: "Alert code", value: code, mono: true });
        if (state) fields.push({ label: "Alert state", value: state.replaceAll("_", " ") });
        if (severity) fields.push({ label: "Severity", value: severity });
        if (key) fields.push({ label: "Target", value: key, mono: true });
        if (typeof obj.to === "string" && obj.to) fields.push({ label: "Owner", value: obj.to });
        if (data) {
          if (typeof data.owner === "string" && data.owner && data.owner !== obj.to) {
            fields.push({ label: "Owner", value: data.owner });
          }
          if (typeof data.class === "string" && data.class) {
            fields.push({ label: "Agent class", value: data.class, mono: true });
          }
          if (typeof data.idle_secs === "number") {
            fields.push({ label: "Idle duration", value: formatDuration(data.idle_secs), mono: true });
          }
          if (typeof data.reason === "string" && data.reason) {
            fields.push({ label: "Reason", value: data.reason });
          }
        }
        if (count !== undefined && count > 1) {
          fields.push({ label: "Occurrences", value: `${count}×`, mono: true });
        }
        if (typeof obj.opened_at === "number" && obj.opened_at > 0) {
          fields.push({ label: "Opened", value: formatEpochTimestamp(obj.opened_at), mono: true });
        }
        if (typeof obj.last_at === "number" && obj.last_at > 0) {
          fields.push({ label: "Last updated", value: formatEpochTimestamp(obj.last_at), mono: true });
        }

        // If it's a generic non-alert JSON object, expose its top-level scalar fields cleanly.
        if (fields.length === 0) {
          for (const [k, v] of Object.entries(obj)) {
            if (v === null || v === undefined || v === "") continue;
            if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") {
              fields.push({
                label: k.replaceAll("_", " "),
                value: typeof v === "number" && k.endsWith("_secs") ? formatDuration(v) : String(v),
                mono: typeof v === "number",
              });
            }
          }
        }

        return {
          isStructured: true,
          title,
          alertCode: code,
          alertState: state,
          severity,
          count,
          fields,
          rawPretty: JSON.stringify(obj, null, 2),
        };
      }
    } catch {
      // Fall through to plain-text handling.
    }
  }

  return {
    isStructured: false,
    title: humanizeTextDurations(message.body),
    fields: [],
  };
}

/** Checks if a bus message represents an acknowledged or closed alert. */
export function isAckedOrClosedAlert(message: BusMessage): boolean {
  if (message.kind !== "ALERT") return false;
  if (message.refs.some((r) => r === "state:acked" || r === "state:closed")) return true;
  const parsed = parseMessageBody(message);
  return parsed.alertState === "acked" || parsed.alertState === "closed";
}

const GENERIC_DIR_NAMES = new Set(["src", "lib", "crates", "tests", "test", "pkg", "packages", "internal", "cmd"]);

const KNOWN_SCOPE_LABELS: Record<string, string> = {
  "bus/src/agent": "bus/agent",
  "bus/src/intake": "bus/intake",
  "bus/tests": "bus/tests",
};

export function scopePrefixOf(raw: string): string {
  const first = raw.split(",")[0]?.trim() ?? "";
  for (const known of Object.keys(KNOWN_SCOPE_LABELS)) {
    if (first.startsWith(known)) return known;
  }
  const clean = first.replace(/\*+$/g, "").replace(/\/+$/g, "");
  const parts = clean.split("/").filter(Boolean);
  if (parts.length >= 3) {
    // Keep up to the meaningful crate/module directory (skip leading generic "src" if nested).
    if (parts[0] === "src" && parts.length >= 4) {
      return parts.slice(0, 4).join("/");
    }
    return parts.slice(0, 3).join("/");
  }
  if (parts.length >= 2) return parts.slice(0, 2).join("/");
  return clean;
}

/** Builds unique, readable dropdown labels for scope prefixes (avoiding duplicate "src" labels). */
export function buildScopeOptions(tasks: WorkTask[]): { value: string; label: string }[] {
  const prefixes = new Set<string>();
  for (const task of tasks) {
    const p = scopePrefixOf(task.scope);
    if (p) prefixes.add(p);
  }

  const initial = [...prefixes].sort().map((value) => {
    if (KNOWN_SCOPE_LABELS[value]) {
      return { value, parts: value.split("/").filter(Boolean), label: KNOWN_SCOPE_LABELS[value] };
    }
    const parts = value.split("/").filter(Boolean);
    const last = parts[parts.length - 1] ?? value;
    const label =
      GENERIC_DIR_NAMES.has(last.toLowerCase()) && parts.length >= 2
        ? parts.slice(-2).join("/")
        : last;
    return { value, parts, label };
  });

  // Resolve any remaining label collisions by expanding trailing path segments.
  const counts = new Map<string, number>();
  for (const item of initial) {
    counts.set(item.label, (counts.get(item.label) ?? 0) + 1);
  }

  return initial.map((item) => {
    if ((counts.get(item.label) ?? 0) <= 1) {
      return { value: item.value, label: item.label };
    }
    const expanded = item.parts.slice(-3).join("/");
    return { value: item.value, label: expanded || item.value };
  });
}

/** Splits a git ref like `origin/feat/async-precompile` into subtle prefix + primary branch name. */
export function splitRefName(refName: string): { prefix?: string; name: string } {
  const match = /^(refs\/heads\/|refs\/remotes\/origin\/|origin\/)(.+)$/.exec(refName);
  if (match) {
    return { prefix: match[1], name: match[2] };
  }
  return { name: refName };
}

/** Deterministic accent tone key for contributor avatars so real participants get distinct colors. */
export function avatarToneKey(name: string): string {
  const known = name.toLowerCase();
  if (known === "leo" || known === "nora" || known === "sam") return known;
  let hash = 0;
  for (let i = 0; i < known.length; i += 1) {
    hash = (hash * 31 + known.charCodeAt(i)) >>> 0;
  }
  return `p${hash % 6}`;
}
