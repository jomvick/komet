"use client";

import { motion } from "framer-motion";
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

const easing = [0.16, 1, 0.3, 1] as const;

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

// Percent of the square stage: where each agent sits, and the link that
// feeds it into komet at the centre.
const RADIUS = 39;
const nodes = agents.map((agent, i) => {
  const a = (i / agents.length) * Math.PI * 2 - Math.PI / 2;
  const cos = Math.cos(a);
  const sin = Math.sin(a);
  return {
    ...agent,
    x: 50 + RADIUS * cos,
    y: 50 + RADIUS * sin,
    x1: 50 + 15 * cos,
    y1: 50 + 15 * sin,
    x2: 50 + (RADIUS - 8) * cos,
    y2: 50 + (RADIUS - 8) * sin,
  };
});

export default function AgentsOrbit() {
  return (
    <section className="overflow-hidden px-5 py-20 md:px-10 md:py-28">
      <div className="relative grid items-center gap-14 lg:grid-cols-2">
        {/* Orbit — komet at the centre, the agents it drives around it */}
        <motion.div
          initial={{ opacity: 0, scale: 0.94 }}
          whileInView={{ opacity: 1, scale: 1 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.7, ease: easing }}
          className="mx-auto w-full max-w-[420px] lg:max-w-[480px]"
        >
          <div className="group relative aspect-square w-full">
            {/* halo behind the logo */}
            <div
              aria-hidden="true"
              className="pointer-events-none absolute inset-[14%] rounded-full bg-[radial-gradient(circle,rgba(255,255,255,0.09),transparent_68%)]"
            />

            <div className="orbit-spin absolute inset-0">
              <svg
                viewBox="0 0 100 100"
                aria-hidden="true"
                className="absolute inset-0 h-full w-full"
              >
                <defs>
                  <radialGradient
                    id="orbit-link"
                    gradientUnits="userSpaceOnUse"
                    cx="50"
                    cy="50"
                    r={RADIUS}
                  >
                    <stop offset="0%" stopColor="#fff" stopOpacity="0.4" />
                    <stop offset="100%" stopColor="#fff" stopOpacity="0.1" />
                  </radialGradient>
                </defs>
                <circle
                  cx="50"
                  cy="50"
                  r={RADIUS}
                  fill="none"
                  stroke="var(--border)"
                  strokeWidth="1"
                  strokeDasharray="1 6"
                  vectorEffect="non-scaling-stroke"
                />
                {nodes.map((n, i) => (
                  <line
                    key={n.name}
                    x1={n.x1}
                    y1={n.y1}
                    x2={n.x2}
                    y2={n.y2}
                    stroke="url(#orbit-link)"
                    strokeWidth="1.2"
                    strokeDasharray="2 6"
                    vectorEffect="non-scaling-stroke"
                    className="flow-line"
                    style={{ animationDelay: `${i * 0.21}s` }}
                  />
                ))}
              </svg>

              {nodes.map(({ name, Icon, x, y }) => (
                <div
                  key={name}
                  className="absolute"
                  style={{ left: `${x}%`, top: `${y}%`, transform: "translate(-50%, -50%)" }}
                >
                  <div className="orbit-counter relative">
                    <div className="flex size-11 items-center justify-center rounded-full border border-border bg-charcoal/80 text-muted-foreground backdrop-blur-sm transition-colors hover:border-border-strong hover:text-foreground">
                      <Icon size={18} />
                    </div>
                    <span className="absolute left-1/2 top-full mt-2 -translate-x-1/2 whitespace-nowrap text-[10.5px] text-muted-foreground/80">
                      {name}
                    </span>
                  </div>
                </div>
              ))}
            </div>

            {/* centre — komet, the control room */}
            <div className="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2">
              <img
                src="/komet.png"
                alt="komet"
                className="size-[84px] rounded-2xl object-contain shadow-[0_0_50px_rgba(255,255,255,0.14)] md:size-[100px]"
              />
            </div>
          </div>

          <p className="mt-2 text-center text-[12px] text-muted-foreground/70">
            + more agents coming
          </p>
        </motion.div>

        {/* Copy */}
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
            Cursor, Devin, Grok, Hermes, Pi, OpenCode, Cline, Qoder,
            Antigravity, Muse and GitHub Copilot — the same sessions and
            tools you use every day, in one fast native window.
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
