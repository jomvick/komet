"use client";

import { motion } from "framer-motion";
import SessionCard from "./SessionCard";

const easing = [0.16, 1, 0.3, 1] as const;

export default function Hero() {
  return (
    <section className="relative overflow-hidden pt-40 pb-24 md:pt-48 md:pb-32">
      <div className="noise pointer-events-none absolute inset-0 opacity-40" />
      <div className="pointer-events-none absolute top-[-20%] left-1/2 -translate-x-1/2 h-[600px] w-[900px] rounded-full bg-signal/10 blur-[120px]" />

      <div className="mx-auto max-w-6xl px-5 grid lg:grid-cols-[1.1fr_0.9fr] gap-16 items-center">
        <div>
          <motion.div
            initial={{ opacity: 0, y: 12 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.6, ease: easing }}
            className="inline-flex items-center gap-2 rounded-full hairline bg-charcoal/70 px-3 py-1 mb-6"
          >
            <span className="h-1.5 w-1.5 rounded-full bg-moss" />
            <span className="font-mono text-[11px] text-mist">
              100% local par défaut
            </span>
          </motion.div>

          <motion.h1
            initial={{ opacity: 0, y: 18 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, delay: 0.05, ease: easing }}
            className="text-[40px] sm:text-[52px] md:text-[60px] leading-[1.04] tracking-tight font-medium text-paper"
          >
            Un seul poste de
            <br />
            contrôle pour tous
            <br />
            tes <span className="text-signal">agents de code</span>.
          </motion.h1>

          <motion.p
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, delay: 0.15, ease: easing }}
            className="mt-6 max-w-lg text-[16px] leading-relaxed text-mist"
          >
            Komet pilote les CLI d&apos;agents que tu utilises déjà —
            sessions, transcripts, activité des outils et checkpoints, dans
            une seule fenêtre native en graphite, entièrement sur ta machine.
          </motion.p>

          <motion.div
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, delay: 0.25, ease: easing }}
            className="mt-9 flex flex-wrap items-center gap-3"
          >
            <a
              href="#downloads"
              className="rounded-[var(--radius-control)] bg-paper px-5 py-2.5 text-[13.5px] font-medium text-ink transition-transform hover:-translate-y-0.5"
            >
              Télécharger komet
            </a>
            <a
              href="https://github.com/opencode/komet"
              target="_blank"
              rel="noreferrer"
              className="rounded-[var(--radius-control)] hairline px-5 py-2.5 text-[13.5px] font-medium text-paper transition-colors hover:bg-graphite/50"
            >
              Voir le code source
            </a>
          </motion.div>

          <motion.p
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: 0.7, delay: 0.4 }}
            className="mt-5 font-mono text-[12px] text-fog"
          >
            curl -fsSL https://komet.sh/install.sh | sh
          </motion.p>
        </div>

        <div className="flex justify-center lg:justify-end">
          <SessionCard />
        </div>
      </div>
    </section>
  );
}
