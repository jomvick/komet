"use client";

import { motion } from "framer-motion";

const easing = [0.16, 1, 0.3, 1] as const;

const nodes = [
  {
    label: "Ton laptop",
    detail: "moteur local, sessions sur disque",
    active: true,
  },
  {
    label: "Relais de sync",
    detail: "CRDT Loro · désactivé par défaut",
    active: false,
  },
  {
    label: "Autre appareil",
    detail: "VPS, desktop, ou rien du tout",
    active: false,
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
          className="max-w-xl mb-14"
        >
          <p className="flex items-center gap-2 font-mono text-[11px] uppercase tracking-wide text-fog mb-3">
            <span className="h-1 w-3 bg-paper/50" />
            Topologie
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Local tout seul, ou synchronisé quand tu l&apos;actives.
          </h2>
          <p className="mt-4 text-[14px] leading-relaxed text-mist">
            Aucun appareil n&apos;est requis en plus du tien. Le relais de
            sync n&apos;existe que si tu le branches explicitement.
          </p>
        </motion.div>

        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="hairline rounded-[var(--radius-panel)] bg-ash/40 px-6 py-10 md:px-10"
        >
          <div className="flex flex-col md:flex-row items-stretch md:items-center gap-6 md:gap-0">
            {nodes.map((n, i) => (
              <div key={n.label} className="flex flex-1 items-center">
                <div
                  className={`flex-1 rounded-[var(--radius-control)] hairline px-5 py-4 ${
                    n.active ? "bg-charcoal" : "bg-transparent"
                  }`}
                >
                  <div className="flex items-center gap-2">
                    <span
                      className={`h-1.5 w-1.5 rounded-full ${
                        n.active ? "bg-paper" : "bg-graphite hairline"
                      }`}
                    />
                    <span className="text-[13.5px] font-medium text-paper">
                      {n.label}
                    </span>
                  </div>
                  <p className="mt-1.5 font-mono text-[11px] text-fog leading-relaxed">
                    {n.detail}
                  </p>
                </div>

                {i < nodes.length - 1 && (
                  <div className="hidden md:block relative w-12 h-px mx-3 bg-[var(--hairline-strong)] shrink-0">
                    <motion.span
                      animate={{ left: ["0%", "100%"] }}
                      transition={{
                        duration: 2.4,
                        repeat: Infinity,
                        ease: "easeInOut",
                        delay: i * 0.3,
                      }}
                      className="absolute -top-[3px] h-[7px] w-[7px] rounded-full bg-paper/70"
                    />
                  </div>
                )}
              </div>
            ))}
          </div>
        </motion.div>
      </div>
    </section>
  );
}
