"use client";

import {
  ClaudeCode,
  Codex,
  Cursor,
  Grok,
  HermesAgent,
  OpenCode,
  Pi,
} from "@lobehub/icons";

const agents = [
  { name: "Claude Code", Icon: ClaudeCode },
  { name: "Codex", Icon: Codex },
  { name: "Cursor", Icon: Cursor },
  { name: "Grok", Icon: Grok },
  { name: "Hermes", Icon: HermesAgent },
  { name: "OpenCode", Icon: OpenCode },
  { name: "Pi", Icon: Pi },
];

export default function AgentRow() {
  return (
    <div>
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase mb-4">
        Drives the agents you already use
      </div>

      <div className="flex flex-wrap items-center gap-x-7 gap-y-4">
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

