"use client";

import {
  Antigravity,
  ClaudeCode,
  Cline,
  Codex,
  Copilot,
  Cursor,
  Devin,
  Grok,
  HermesAgent,
  Meta,
  OpenCode,
  Pi,
  Qoder,
} from "@lobehub/icons";

const agents = [
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
  { name: "Muse", Icon: Meta },
  { name: "GitHub Copilot", Icon: Copilot },
];

export default function AgentRow() {
  return (
    <div>
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase mb-4">
        Drives {agents.length} agents you already use
      </div>

      <div className="flex flex-wrap items-center gap-x-6 gap-y-4">
        {agents.map(({ name, Icon }) => (
          <span
            key={name}
            title={name}
            className="text-muted-foreground/70 transition-colors hover:text-foreground"
          >
            <Icon size={22} />
          </span>
        ))}
      </div>
    </div>
  );
}
