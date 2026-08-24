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
      <p className="font-mono text-[11px] uppercase tracking-wide text-fog mb-4">
        Pilote les agents que tu as déjà
      </p>

      <div className="flex flex-wrap items-center gap-6">
        {agents.map(({ name, Icon }) => (
          <span
            key={name}
            title={name}
            className="text-fog transition-colors hover:text-mist"
          >
            <Icon size={18} />
          </span>
        ))}
      </div>
    </div>
  );
}
