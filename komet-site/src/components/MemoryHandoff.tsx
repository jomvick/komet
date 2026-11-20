"use client";

import { motion } from "framer-motion";
import { ClaudeCode, Codex } from "@lobehub/icons";
import { Database } from "lucide-react";

const easing = [0.16, 1, 0.3, 1] as const;

// Shape of the real briefing the engine injects at the harness boundary
// (`komet_memory::briefing::build`), trimmed to a few lines.
const briefing = [
  "Komet project memory — locally stored project context.",
  "3 chat(s) across 2 agent(s).",
  "- Codex — 2 runs, 41 tool calls, 2h ago; modified src/auth/session.rs",
  "- Claude Code — 1 run, 12 tool calls, 1d ago; modified src/engine/run.rs",
  "Known facts: auth uses the run journal [supported; source codex]",
];

function Connector() {
  return (
    <svg
      className="h-2 min-w-0 flex-1"
      viewBox="0 0 100 8"
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      <line
        x1="0"
        y1="4"
        x2="100"
        y2="4"
        stroke="var(--border-strong)"
        strokeWidth="1"
        strokeDasharray="2 5"
        vectorEffect="non-scaling-stroke"
        className="flow-line"
      />
    </svg>
  );
}

export default function MemoryHandoff() {
  return (
    <section className="px-5 py-20 md:px-10 md:py-28">
      <div className="grid items-center gap-14 lg:grid-cols-2">
        {/* Copy */}
        <motion.div
          initial={{ opacity: 0, y: 14 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
        >
          <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
            Shared memory
          </div>
          <h2 className="mt-3 text-[30px] leading-tight font-medium tracking-tight text-foreground md:text-[36px]">
            What one agent learns, the next one knows
          </h2>
          <p className="mt-4 max-w-md text-[15px] leading-relaxed text-muted-foreground">
            Komet keeps a local, allow-listed trace of every run — files
            touched, tool calls, failures, decisions — and hands it to the next
            agent as a briefing when it starts on the same project. Claude picks
            up where Codex stopped, without you retelling the story.
          </p>
          <ul className="mt-6 space-y-2 text-[13.5px] leading-relaxed text-muted-foreground">
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Event metadata only: never prompts, file contents or diffs.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              SQLite + FTS5 on your disk, 180-day retention, no server.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Injected once at session start, capped at ~400 tokens.
            </li>
          </ul>
        </motion.div>

        {/* Visual: the handoff, then the briefing the next agent actually receives */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.7, ease: easing }}
          className="rounded-2xl border border-border bg-charcoal/30 p-5 md:p-6"
        >
          <div className="flex items-center gap-2 md:gap-3">
            <div className="flex w-[74px] shrink-0 flex-col items-center gap-1.5">
              <div className="flex size-10 items-center justify-center rounded-full border border-border bg-charcoal/70 text-muted-foreground">
                <Codex size={16} />
              </div>
              <span className="text-[10.5px] text-muted-foreground/80">Codex</span>
            </div>

            <Connector />

            <div className="relative flex shrink-0 flex-col items-center gap-1.5">
              <div className="relative flex size-12 items-center justify-center rounded-full border border-border-strong bg-background">
                <Database className="size-5 text-foreground/80" strokeWidth={1.5} />
                <span
                  aria-hidden="true"
                  className="pulse-soft absolute -inset-1 rounded-full border border-white/10"
                />
              </div>
              <span className="font-mono text-[9.5px] tracking-wide text-muted-foreground/70 uppercase">
                local
              </span>
            </div>

            <Connector />

            <div className="flex w-[74px] shrink-0 flex-col items-center gap-1.5">
              <div className="flex size-10 items-center justify-center rounded-full border border-border bg-charcoal/70 text-muted-foreground">
                <ClaudeCode size={16} />
              </div>
              <span className="text-[10.5px] text-muted-foreground/80">Claude Code</span>
            </div>
          </div>

          <div className="mt-5 rounded-xl border border-border bg-background/60 p-4">
            <div className="mb-2 flex items-center justify-between font-mono text-[9.5px] tracking-[0.14em] text-muted-foreground/60 uppercase">
              <span>briefing injected</span>
              <span>~410 tokens</span>
            </div>
            <div className="space-y-1 font-mono text-[11px] leading-relaxed break-words text-muted-foreground">
              {briefing.map((line, i) => (
                <motion.p
                  key={line}
                  initial={{ opacity: 0 }}
                  whileInView={{ opacity: 1 }}
                  viewport={{ once: true, margin: "-40px" }}
                  transition={{ duration: 0.35, delay: 0.3 + i * 0.12 }}
                  className={i === 0 ? "text-foreground/80" : undefined}
                >
                  {line}
                </motion.p>
              ))}
            </div>
          </div>
        </motion.div>
      </div>
    </section>
  );
}
