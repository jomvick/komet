"use client";

import { motion } from "framer-motion";
import { Laptop2, Server, ShieldCheck } from "lucide-react";

const easing = [0.16, 1, 0.3, 1] as const;

const destinations = [
  {
    label: "This machine",
    detail: "sessions · memory · credentials, on disk",
    tag: "default",
    Icon: Laptop2,
    y: 16.5,
  },
  {
    label: "Sync relay",
    detail: "Loro CRDT · encrypted · your server",
    tag: "opt-in",
    Icon: ShieldCheck,
    y: 50,
  },
  {
    label: "Other device",
    detail: "VPS or second machine",
    tag: "opt-in",
    Icon: Server,
    y: 83.5,
  },
];

export default function Topology() {
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
            Local-first
          </div>
          <h2 className="mt-3 text-[30px] leading-tight font-medium tracking-tight text-foreground md:text-[36px]">
            One binary. Your machine is home base.
          </h2>
          <p className="mt-4 max-w-md text-[15px] leading-relaxed text-muted-foreground">
            Komet is a single Rust binary — the same engine runs headed in the
            window or headless on a server. Projects, sessions, shared memory
            and credentials live on your disk: no account, no telemetry, no
            cloud between you and your agents.
          </p>
          <ul className="mt-6 space-y-2 text-[13.5px] leading-relaxed text-muted-foreground">
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Works fully offline; SQLite store under your data directory.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Self-hosted sync built in — Loro CRDT, per-device relay,
              encrypted — off by default, enabled with komet sync-init.
            </li>
            <li className="flex gap-2.5">
              <span className="text-muted-foreground/40">—</span>
              Headless mode keeps agents running on a VPS while your laptop
              sleeps.
            </li>
          </ul>
        </motion.div>

        {/* Visual: komet fans out to where sessions can live */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.7, ease: easing }}
          className="rounded-2xl border border-border bg-charcoal/30 p-5 md:p-6"
        >
          <div className="flex items-center justify-between">
            <p className="text-[13px] font-medium text-foreground">komet</p>
            <p className="font-mono text-[10.5px] text-muted-foreground/60">
              one binary · headed or headless
            </p>
          </div>

          <div className="relative mt-5 h-[280px]">
            <svg
              className="absolute inset-0 h-full w-full"
              viewBox="0 0 100 100"
              preserveAspectRatio="none"
              aria-hidden="true"
            >
              {destinations.map((d, i) => (
                <path
                  key={d.label}
                  d={`M 30,${d.y} C 40,${d.y} 44,${d.y} 58,${d.y}`}
                  fill="none"
                  stroke="var(--border-strong)"
                  strokeWidth="1"
                  strokeDasharray="2 5"
                  vectorEffect="non-scaling-stroke"
                  className="flow-line"
                  style={{ animationDelay: `${i * 0.3}s` }}
                />
              ))}
            </svg>

            {/* centre-left — the binary itself */}
            <div className="absolute left-0 top-1/2 w-[26%] -translate-y-1/2">
              <div className="flex flex-col items-center gap-2 rounded-xl border border-border-strong bg-background px-3 py-4 text-center">
                <img
                  src="/komet.png"
                  alt=""
                  className="size-9 rounded-lg object-contain"
                />
                <p className="text-[12px] font-medium text-foreground">
                  komet
                </p>
                <p className="font-mono text-[9px] tracking-wide text-muted-foreground/60 uppercase">
                  engine + ui
                </p>
              </div>
            </div>

            {/* right — where sessions can live */}
            <div className="absolute right-0 top-0 flex h-full w-[62%] flex-col justify-between py-1">
              {destinations.map(({ label, detail, tag, Icon }) => (
                <div
                  key={label}
                  className="flex items-center gap-3 rounded-xl border border-border bg-charcoal/70 px-4 py-3"
                >
                  <Icon
                    className="size-4 shrink-0 text-muted-foreground"
                    strokeWidth={1.6}
                  />
                  <div className="min-w-0 flex-1">
                    <p className="text-[13px] leading-tight text-foreground">
                      {label}
                    </p>
                    <p className="mt-0.5 truncate font-mono text-[10px] leading-tight text-muted-foreground/60">
                      {detail}
                    </p>
                  </div>
                  <span className="shrink-0 font-mono text-[9px] tracking-wide text-muted-foreground/50 uppercase">
                    {tag}
                  </span>
                </div>
              ))}
            </div>
          </div>

          <p className="mt-4 font-mono text-[10px] leading-relaxed text-muted-foreground/50">
            Sync stays off until you point it at a VPS running the self-hosted
            sync server.
          </p>
        </motion.div>
      </div>
    </section>
  );
}

