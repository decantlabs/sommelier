import type { AgentProvider } from "../api/types";

// Static marks from @lobehub/icons-static-svg 1.106.0. License is bundled with the assets.
const PROVIDER_FILES: Record<AgentProvider, string> = {
  Claude: "claude-color.svg",
  Codex: "openai.svg",
  Gemini: "gemini-color.svg",
};

function matchProviderText(text: string): AgentProvider | undefined {
  const lower = text.toLowerCase();
  if (!lower) return undefined;
  if (
    lower.includes("claude") ||
    lower.includes("sonnet") ||
    lower.includes("opus") ||
    lower.includes("haiku")
  ) {
    return "Claude";
  }
  if (
    lower.includes("codex") ||
    lower.includes("openai") ||
    lower.includes("gpt") ||
    /\bo[134](?:-|$|\b)/.test(lower)
  ) {
    return "Codex";
  }
  if (lower.includes("gemini")) {
    return "Gemini";
  }
  return undefined;
}

/**
 * Infers the AI model provider using the most specific signal first:
 * 1. Explicit model string
 * 2. Agent/worker name (e.g. `opus-queue-async`, `sonnet-dstorage-fast`, `claude-prewarm`)
 * 3. Agent class (only falls back to `agy` -> Gemini if neither model nor name specifies another family)
 */
export function inferProvider(model?: string, cls?: string, name?: string): AgentProvider | undefined {
  const fromModel = matchProviderText(model ?? "");
  if (fromModel) return fromModel;

  const fromName = matchProviderText(name ?? "");
  if (fromName) return fromName;

  const fromClass = matchProviderText(cls ?? "");
  if (fromClass) return fromClass;

  const clsLower = (cls ?? "").toLowerCase();
  if (clsLower.includes("agy")) {
    return "Gemini";
  }
  return undefined;
}

export function ProviderMark({ provider, size = 16 }: { provider: AgentProvider; size?: number }) {
  return (
    <img
      className="provider-mark"
      data-provider={provider}
      src={`${import.meta.env.BASE_URL}assets/providers/${PROVIDER_FILES[provider]}`}
      width={size}
      height={size}
      alt=""
      aria-hidden="true"
    />
  );
}

export function ProviderLabel({
  provider,
  size = 16,
  suffix,
}: {
  provider: AgentProvider;
  size?: number;
  suffix?: string;
}) {
  return (
    <span className="provider-label">
      <ProviderMark provider={provider} size={size} />
      <span>
        {provider}
        {suffix ?? ""}
      </span>
    </span>
  );
}
