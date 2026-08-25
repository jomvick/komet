"use client";

import { motion } from "framer-motion";
import { ClaudeCode, Codex, Cursor, OpenCode } from "@lobehub/icons";
import { ShieldCheck, Laptop2, Server } from "lucide-react";

const easing = [0.16, 1, 0.3, 1] as const;

const agents = [
  { label: "Claude Code", Icon: ClaudeCode, y: 12.5 },
  { label: "Codex", Icon: Codex, y: 37.5 },
  { label: "Cursor", Icon: Cursor, y: 62.5 },
  { label: "OpenCode", Icon: OpenCode, y: 87.5 },
];

const destinations = [
  {
    label: "This machine",
    detail: "local by default",
    Icon: Laptop2,
    y: 16.5,
  },
  {
    label: "Sync relay",
    detail: "Loro CRDT, encrypted, opt-in",
    Icon: ShieldCheck,
    y: 50,
  },
  {
    label: "Other device",
    detail: "VPS or second machine",
    Icon: Server,
    y: 83.5,
  },
];

export default function Topology() {
  return (
    <section className="py-24 md:py-32 border-t border-[var(--hairline)]">
      <div className="mx-auto max-w-6xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="max-w-xl mb-16"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-fog mb-3">
            Topologie
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Runs where you work.
          </h2>
          <p className="mt-4 text-[15px] leading-relaxed text-mist max-w-lg">
            komet drives your agents locally. No extra device
            is required — the sync relay only exists if you
            explicitly plug it in.
          </p>
        </motion.div>

        {/* Desktop: fan diagram */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="hidden md:block relative h-[380px]"
        >
          <svg
            className="absolute inset-0 h-full w-full"
            viewBox="0 0 100 100"
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            {agents.map((a) => (
              <path
                key={a.label}
                d={`M 20,${a.y} C 35,${a.y} 35,50 50,50`}
                fill="none"
                stroke="var(--hairline-strong)"
                strokeWidth="0.3"
                strokeDasharray="1.2 1.4"
                vectorEffect="non-scaling-stroke"
              />
            ))}
            {destinations.map((d) => (
              <path
                key={d.label}
                d={`M 50,50 C 65,50 65,${d.y} 80,${d.y}`}
                fill="none"
                stroke="var(--hairline-strong)"
                strokeWidth="0.3"
                strokeDasharray="1.2 1.4"
                vectorEffect="non-scaling-stroke"
              />
            ))}
          </svg>

          {/* left column — agents */}
          <div className="absolute left-0 top-0 h-full w-[180px] flex flex-col justify-between py-1">
            {agents.map(({ label, Icon }) => (
              <div
                key={label}
                className="flex items-center gap-3 rounded-[var(--radius-control)] hairline bg-charcoal/70 px-4 py-3"
              >
                <Icon size={16} className="text-mist shrink-0" />
                <span className="text-[13px] text-paper">{label}</span>
              </div>
            ))}
          </div>

          {/* center — komet */}
          <div className="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 w-[190px]">
            <div className="rounded-[var(--radius-control)] hairline bg-graphite/60 px-5 py-4 text-center">
              <p className="text-[13.5px] font-medium text-paper">komet</p>
              <p className="mt-0.5 font-mono text-[10.5px] text-fog">
                control room
              </p>
            </div>
          </div>

          {/* right column — destinations */}
          <div className="absolute right-0 top-0 h-full w-[220px] flex flex-col justify-between py-1">
            {destinations.map(({ label, detail, Icon }) => (
              <div
                key={label}
                className="flex items-center gap-3 rounded-[var(--radius-control)] hairline bg-charcoal/70 px-4 py-3"
              >
                <Icon className="h-4 w-4 text-mist shrink-0" strokeWidth={1.6} />
                <div>
                  <p className="text-[13px] text-paper leading-tight">{label}</p>
                  <p className="font-mono text-[10.5px] text-fog leading-tight">
                    {detail}
                  </p>
                </div>
              </div>
            ))}
          </div>
        </motion.div>

        {/* Small screens: simple stacked flow */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="md:hidden hairline rounded-[var(--radius-panel)] bg-ash/40 divide-y divide-[var(--hairline)]"
        >
          <div className="px-5 py-4">
            <p className="font-mono text-[10.5px] uppercase tracking-wide text-fog mb-2">
              Agents driven
            </p>
            <p className="text-[13px] text-mist">
              Claude Code, Codex, Cursor, OpenCode…
            </p>
          </div>
          <div className="px-5 py-4">
            <p className="text-[13.5px] font-medium text-paper">komet</p>
            <p className="font-mono text-[10.5px] text-fog">local control room</p>
          </div>
          <div className="px-5 py-4">
            <p className="font-mono text-[10.5px] uppercase tracking-wide text-fog mb-2">
              Where your sessions live
            </p>
            <p className="text-[13px] text-mist">
              This machine by default, or an encrypted sync relay if you
              turn it on.
            </p>
          </div>
        </motion.div>
      </div>
    </section>
  );
}
