import {
  Antigravity,
  ClaudeCode,
  Cline,
  Codex,
  Cursor,
  Devin,
  GithubCopilot,
  Grok,
  HermesAgent,
  OpenCode,
  Pi,
  Qoder,
} from "@lobehub/icons";

// The 13 harnesses the engine actually drives (`HarnessId`, minus `Mock`).
// Muse has no mark in @lobehub/icons — it renders as a monogram.
export const allAgents = [
  { name: "Claude Code", Icon: ClaudeCode },
  { name: "Codex", Icon: Codex },
  { name: "Cursor", Icon: Cursor },
  { name: "Devin", Icon: Devin },
  { name: "Grok", Icon: Grok },
  { name: "Hermes", Icon: HermesAgent },
  { name: "Pi", Icon: Pi },
  { name: "OpenCode", Icon: OpenCode },
  { name: "Cline", Icon: Cline },
  { name: "Qoder", Icon: Qoder },
  { name: "Antigravity", Icon: Antigravity },
  { name: "Muse", Icon: null },
  { name: "GitHub Copilot", Icon: GithubCopilot },
] as const;
