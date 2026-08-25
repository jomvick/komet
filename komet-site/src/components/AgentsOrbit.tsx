"use client";

import { motion } from "framer-motion";
import {
  ClaudeCode,
  Codex,
  Cursor,
  Grok,
  HermesAgent,
  OpenCode,
  Pi,
} from "@lobehub/icons";

const easing = [0.16, 1, 0.3, 1] as const;

const agents = [
  { name: "Claude Code", Icon: ClaudeCode },
  { name: "Codex", Icon: Codex },
  { name: "Cursor", Icon: Cursor },
  { name: "Grok", Icon: Grok },
  { name: "Hermes", Icon: HermesAgent },
  { name: "OpenCode", Icon: OpenCode },
  { name: "Pi", Icon: Pi },
];

export default function AgentsOrbit() {
  return (
    <section className="px-5 py-20 md:px-10 md:py-28 overflow-hidden">
      <div className="relative grid items-center gap-14 lg:grid-cols-2">
        {/* Agent cards - vertical stack */}
        <div className="mx-auto flex w-full max-w-md flex-col gap-3">
          {agents.map(({ name, Icon }, i) => (
            <motion.div
              key={name}
              initial={{ opacity: 0, y: 14 }}
              whileInView={{ opacity: 1, y: 0 }}
              viewport={{ once: true, margin: "-80px" }}
              transition={{ duration: 0.45, delay: 0.1 + i * 0.07, ease: easing }}
              className="flex h-14 items-center justify-center gap-3 rounded-xl border border-border bg-charcoal/40 text-[15px] font-medium text-foreground transition-colors hover:border-border-strong hover:bg-charcoal/70"
            >
              <Icon size={17} className="text-muted-foreground" />
              {name}
            </motion.div>
          ))}
          <motion.div
            initial={{ opacity: 0 }}
            whileInView={{ opacity: 1 }}
            viewport={{ once: true, margin: "-80px" }}
            transition={{ duration: 0.45, delay: 0.1 + agents.length * 0.07, ease: easing }}
            className="flex h-14 items-center justify-center rounded-xl border border-dashed border-border text-sm text-muted-foreground"
          >
            + more agents coming
          </motion.div>
        </div>

        {/* Copy - same style as Hero/Features */}
        <motion.div
          initial={{ opacity: 0, y: 14 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
        >
          <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
            Compatible
          </div>
          <h2 className="mt-3 text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-foreground">
            Drives what you already use
          </h2>
          <p className="mt-4 max-w-md text-[15px] leading-relaxed text-muted-foreground">
            Komet is not a new agent. It drives Claude Code, Codex,
            Cursor, Grok, Hermes, OpenCode and Pi — the same sessions and tools
            you use every day, in one fast native window.
          </p>
          <a
            href="https://github.com/jomvick/komet"
            target="_blank"
            rel="noreferrer"
            className="mt-7 inline-flex h-10 items-center gap-1.5 rounded-lg border border-border bg-transparent px-4 text-sm font-medium text-foreground hover:border-border-strong hover:bg-charcoal/40 transition-colors"
          >
            Star on GitHub
            <span aria-hidden="true">↗</span>
          </a>
        </motion.div>
      </div>
    </section>
  );
}
