"use client";

import { motion } from "framer-motion";
import { features } from "@/lib/data";

const easing = [0.16, 1, 0.3, 1] as const;

export default function Features() {
  return (
    <section id="features" className="py-24 md:py-32">
      <div className="mx-auto max-w-6xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="max-w-xl mb-14"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-signal mb-3">
            Pourquoi natif
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Construit comme un vrai poste de pilotage, pas comme un chat de plus.
          </h2>
        </motion.div>

        <div className="grid sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {features.map((f, i) => (
            <motion.div
              key={f.title}
              initial={{ opacity: 0, y: 20 }}
              whileInView={{ opacity: 1, y: 0 }}
              viewport={{ once: true, margin: "-60px" }}
              transition={{ duration: 0.55, delay: (i % 3) * 0.08, ease: easing }}
              className="group hairline rounded-[var(--radius-panel)] bg-ash/60 p-6 transition-colors hover:bg-ash"
            >
              <span className="font-mono text-[11px] text-signal">{f.tag}</span>
              <h3 className="mt-3 text-[16.5px] font-medium text-paper leading-snug">
                {f.title}
              </h3>
              <p className="mt-2.5 text-[13.5px] leading-relaxed text-mist">
                {f.body}
              </p>
            </motion.div>
          ))}
        </div>
      </div>
    </section>
  );
}
