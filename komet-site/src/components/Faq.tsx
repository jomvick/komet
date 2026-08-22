"use client";

import { useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { faq } from "@/lib/data";

const easing = [0.16, 1, 0.3, 1] as const;

export default function Faq() {
  const [open, setOpen] = useState<number | null>(0);

  return (
    <section id="faq" className="py-24 md:py-32 border-t border-[var(--hairline)]">
      <div className="mx-auto max-w-3xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="mb-12"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-signal mb-3">
            Questions
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Ce qu&apos;on te demande le plus.
          </h2>
        </motion.div>

        <div className="space-y-2">
          {faq.map((item, i) => {
            const isOpen = open === i;
            return (
              <motion.div
                key={item.q}
                initial={{ opacity: 0, y: 12 }}
                whileInView={{ opacity: 1, y: 0 }}
                viewport={{ once: true, margin: "-40px" }}
                transition={{ duration: 0.45, delay: i * 0.05 }}
                className="hairline rounded-[var(--radius-panel)] bg-ash/50 overflow-hidden"
              >
                <button
                  onClick={() => setOpen(isOpen ? null : i)}
                  className="w-full flex items-center justify-between gap-4 px-5 py-4 text-left"
                >
                  <span className="text-[14.5px] font-medium text-paper">
                    {item.q}
                  </span>
                  <motion.span
                    animate={{ rotate: isOpen ? 45 : 0 }}
                    transition={{ duration: 0.25 }}
                    className="shrink-0 text-[18px] text-fog leading-none"
                  >
                    +
                  </motion.span>
                </button>
                <AnimatePresence initial={false}>
                  {isOpen && (
                    <motion.div
                      initial={{ height: 0, opacity: 0 }}
                      animate={{ height: "auto", opacity: 1 }}
                      exit={{ height: 0, opacity: 0 }}
                      transition={{ duration: 0.3, ease: easing }}
                      className="overflow-hidden"
                    >
                      <p className="px-5 pb-4 text-[13.5px] leading-relaxed text-mist">
                        {item.a}
                      </p>
                    </motion.div>
                  )}
                </AnimatePresence>
              </motion.div>
            );
          })}
        </div>
      </div>
    </section>
  );
}
