"use client";

import { motion } from "framer-motion";
import { features } from "@/lib/data";

const easing = [0.16, 1, 0.3, 1] as const;

export default function Features() {
  return (
    <section id="features" className="py-24 md:py-32 border-t border-[var(--hairline)]">
      <div className="mx-auto max-w-6xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="max-w-xl mb-16"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-fog mb-3">
            Pourquoi natif
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Construit comme un vrai poste de pilotage, pas comme un chat de plus.
          </h2>
        </motion.div>

        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="divide-y divide-[var(--hairline)] border-t border-[var(--hairline)]"
        >
          {features.map((f, i) => (
            <div
              key={f.title}
              className="grid md:grid-cols-[80px_1fr_1.4fr] gap-2 md:gap-8 py-7"
            >
              <span className="font-mono text-[12px] text-fog">
                {String(i + 1).padStart(2, "0")}
              </span>
              <h3 className="text-[16px] font-medium text-paper leading-snug">
                {f.title}
              </h3>
              <p className="text-[13.5px] leading-relaxed text-mist max-w-xl">
                {f.body}
              </p>
            </div>
          ))}
        </motion.div>
      </div>
    </section>
  );
}
