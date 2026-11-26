import type { ReactNode } from "react";

export type StatusTone = "accent" | "good" | "warning" | "danger" | "muted";
export type StatusVariant = "inline" | "dot" | "pill" | "chip";

export function StatusLabel({
  tone = "muted",
  variant = "inline",
  icon,
  hollowDot = false,
  className = "",
  children,
}: {
  tone?: StatusTone;
  variant?: StatusVariant;
  icon?: ReactNode;
  hollowDot?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <span className={`ui-status ui-status-${variant} ${className}`.trim()} data-tone={tone}>
      {variant === "dot" && (
        <span aria-hidden="true" className={`ui-status-dot-mark ${hollowDot ? "is-hollow" : ""}`} />
      )}
      {icon}
      <span>{children}</span>
    </span>
  );
}

export function taskStateTone(state: string): StatusTone {
  const lower = state.toLowerCase();
  if (lower === "running" || lower === "closed" || lower === "delivered" || lower === "landing") {
    return "good";
  }
  if (lower === "stuck") return "danger";
  if (lower === "draft" || lower === "cancelled") return "muted";
  return "accent";
}

export function submissionStateTone(state: string): StatusTone {
  switch (state) {
    case "landed":
    case "approved":
    case "verified":
    case "gated":
      return "good";
    case "gate_failed":
    case "verify_failed":
    case "review_failed":
    case "rejected":
      return "danger";
    case "reviewing":
    case "verifying":
      return "warning";
    default:
      return "accent";
  }
}

export function agentStateTone(state: string): StatusTone {
  const lower = state.toLowerCase();
  if (lower === "running") return "good";
  if (lower === "review assigned" || lower === "suspect" || lower === "starved" || lower === "draining") {
    return "warning";
  }
  if (lower === "stale" || lower === "blocked" || lower === "dead") return "danger";
  if (lower === "own") return "accent";
  return "muted";
}

export function poolStateTone(state: string): StatusTone {
  switch (state) {
    case "open":
      return "good";
    case "drain":
    case "throttled":
    case "paused":
      return "warning";
    case "closed":
      return "danger";
    default:
      return "muted";
  }
}

export function messageKindTone(kind: string): StatusTone {
  switch (kind) {
    case "FACT":
    case "LANDED":
    case "READY":
    case "ACCEPT":
      return "good";
    case "TASK":
    case "TASK_UPDATE":
    case "OFFER":
      return "accent";
    case "ALERT":
    case "STUCK":
    case "DECLINE":
      return "warning";
    default:
      return "muted";
  }
}

export function featureStateTone(state: string): StatusTone {
  switch (state) {
    case "works":
      return "good";
    case "flaky":
    case "untested":
      return "warning";
    case "broken":
    case "regressed":
      return "danger";
    default:
      return "muted";
  }
}
