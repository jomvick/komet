"use client";

import { motion } from "framer-motion";
import {
  ClaudeCode,
  Codex,
  Cursor,
  Grok,
  HermesAgent,
  OpenCode,
} from "@lobehub/icons";

const easing = [0.16, 1, 0.3, 1] as const;

// Illustrative 30-day figures; the app reads the same totals from each CLI's
// local logs (turns.jsonl) and labels cost as an estimate.
const rows = [
  { name: "Claude Code", Icon: ClaudeCode, runs: 128, tokens: "5.8M", cost: "$21.40", share: 100 },
  { name: "Codex", Icon: Codex, runs: 74, tokens: "3.1M", cost: "$11.20", share: 53 },
  { name: "Cursor", Icon: Cursor, runs: 41, tokens: "1.6M", cost: "$5.10", share: 28 },
  { name: "Grok", Icon: Grok, runs: 28, tokens: "980K", cost: "$2.20", share: 17 },
  { name: "OpenCode", Icon: OpenCode, runs: 19, tokens: "620K", cost: "$1.10", share: 11 },
  { name: "Hermes", Icon: HermesAgent, runs: 12, tokens: "240K", cost: "$0.40", share: 4 },
];

export default function UsageTable() {
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
            Usage
          </div>
          <h2 className="mt-3 text-[30px] leading-tight font-medium tracking-tight text-foreground md:text-[36px]">
            Know where the tokens went
          </h2>
          <p className="mt-4 max-w-md text-[15px] leading-relaxed text-muted-foreground">
            Komet reads the token counters each CLI already writes on disk and
            totals them per agent, per model and per project — 24 hours, 7 days,
            30 days. No API key, no billing access, nothing uploaded.
          </p>
          <ul className="mt-6 space-y-2 text-[13.5px] leading-relaxed text-muted-foreground">
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Measured turns and local logs, kept 180 days on your disk.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Quota windows and rate-limit rejections when the provider exposes
              them.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Cost is an estimate from a dated price table, never a bill. CSV
              export included.
            </li>
          </ul>
        </motion.div>

        {/* Visual: the usage panel */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.7, ease: easing }}
          className="rounded-2xl border border-border bg-charcoal/30 p-5 md:p-6"
        >
          <div className="flex items-start justify-between gap-3">
            <div>
              <p className="text-[13px] font-medium text-foreground">Usage</p>
              <p className="mt-0.5 font-mono text-[10.5px] text-muted-foreground/60">
                13 agents · 30 days
              </p>
            </div>
            <div className="flex rounded-lg border border-border p-0.5 font-mono text-[10px]">
              {["24h", "7d", "30d"].map((period) => (
                <span
                  key={period}
                  className={
                    period === "30d"
                      ? "rounded-[5px] bg-foreground px-2 py-1 text-background"
                      : "px-2 py-1 text-muted-foreground/60"
                  }
                >
                  {period}
                </span>
              ))}
            </div>
          </div>

          <div className="mt-5 flex items-baseline gap-2.5">
            <span className="text-[26px] leading-none font-medium tabular-nums text-foreground">
              12.4M
            </span>
            <span className="text-[11px] text-muted-foreground">
              tokens · 318 runs · $41.50 est.
            </span>
          </div>

          <div className="mt-5">
            <div className="flex items-center gap-3 border-b border-border pb-2 font-mono text-[9.5px] tracking-[0.12em] text-muted-foreground/50 uppercase">
              <span className="w-5" />
              <span className="flex-1">agent</span>
              <span className="w-9 text-right">runs</span>
              <span className="w-12 text-right">tokens</span>
              <span className="w-12 text-right">est.</span>
            </div>

            <div className="divide-y divide-border">
              {rows.map(({ name, Icon, runs, tokens, cost, share }, i) => (
                <div
                  key={name}
                  className="relative flex items-center gap-3 py-2.5 text-[12.5px] text-foreground/85"
                >
                  <motion.span
                    aria-hidden="true"
                    initial={{ width: 0 }}
                    whileInView={{ width: `${share}%` }}
                    viewport={{ once: true, margin: "-40px" }}
                    transition={{ duration: 0.9, delay: 0.15 + i * 0.08, ease: easing }}
                    className="absolute inset-y-0 left-0 rounded-[3px] bg-white/[0.035]"
                  />
                  <span className="relative flex w-5 items-center justify-center text-muted-foreground/70">
                    <Icon size={13} />
                  </span>
                  <span className="relative flex-1 truncate">{name}</span>
                  <span className="relative w-9 text-right font-mono text-[11px] tabular-nums text-muted-foreground">
                    {runs}
                  </span>
                  <span className="relative w-12 text-right font-mono text-[11px] tabular-nums">
                    {tokens}
                  </span>
                  <span className="relative w-12 text-right font-mono text-[11px] tabular-nums text-muted-foreground">
                    {cost}
                  </span>
                </div>
              ))}
            </div>

            <div className="flex items-center gap-3 border-t border-border pt-2.5 text-[11.5px] text-muted-foreground/50">
              <span className="w-5" />
              <span className="flex-1">7 more agents</span>
              <span className="w-9 text-right font-mono tabular-nums">16</span>
              <span className="w-12 text-right font-mono tabular-nums">60K</span>
              <span className="w-12 text-right font-mono tabular-nums">$0.10</span>
            </div>
          </div>

          <p className="mt-4 font-mono text-[10px] leading-relaxed text-muted-foreground/50">
            Measured from each CLI&apos;s local logs · cost is an estimate, not a
            bill
          </p>
        </motion.div>
      </div>
    </section>
  );
}
