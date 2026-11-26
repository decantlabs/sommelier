import type { AgentProvider } from "../api/types";

// Static marks from @lobehub/icons-static-svg 1.106.0. License is bundled with the assets.
const PROVIDER_FILES: Record<AgentProvider, string> = {
  Claude: "claude-color.svg",
  Codex: "openai.svg",
  Gemini: "gemini-color.svg",
};

export function inferProvider(model?: string, cls?: string): AgentProvider | undefined {
  const combined = `${model ?? ""} ${cls ?? ""}`.toLowerCase();
  if (combined.includes("codex") || combined.includes("openai") || combined.includes("gpt")) {
    return "Codex";
  }
  if (combined.includes("gemini") || combined.includes("agy")) {
    return "Gemini";
  }
  if (combined.includes("claude") || combined.includes("sonnet") || combined.includes("opus") || combined.includes("haiku")) {
    return "Claude";
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
