"use client";

import { motion } from "framer-motion";
import { stats } from "@/lib/data";

const easing = [0.16, 1, 0.3, 1] as const;

export default function Stats() {
  return (
    <section id="speed" className="py-24 md:py-32 border-y border-[var(--hairline)] bg-charcoal/30">
      <div className="mx-auto max-w-6xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="max-w-xl mb-14"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-signal mb-3">
            Bâti pour la vitesse
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Aucune couche à traverser entre toi et l&apos;agent.
          </h2>
        </motion.div>

        <div className="grid sm:grid-cols-2 lg:grid-cols-4 gap-4">
          {stats.map((s, i) => (
            <motion.div
              key={s.label}
              initial={{ opacity: 0, y: 20 }}
              whileInView={{ opacity: 1, y: 0 }}
              viewport={{ once: true, margin: "-60px" }}
              transition={{ duration: 0.55, delay: i * 0.08, ease: easing }}
              className="hairline rounded-[var(--radius-panel)] bg-ash/50 p-6"
            >
              <p className="font-mono text-[26px] text-paper tracking-tight">
                {s.value}
              </p>
              <p className="mt-1 text-[13px] font-medium text-mist">
                {s.label}
              </p>
              <p className="mt-2 text-[12px] leading-relaxed text-fog">
                {s.note}
              </p>
            </motion.div>
          ))}
        </div>
      </div>
    </section>
  );
}
