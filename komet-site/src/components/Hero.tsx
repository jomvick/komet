"use client";

import { motion } from "framer-motion";
import SessionCard from "./SessionCard";
import AgentRow from "./AgentRow";

export default function Hero() {
  return (
    <section className="relative overflow-hidden pt-20 pb-24 md:pt-28 md:pb-32">
      <div className="mx-auto max-w-6xl px-5 grid lg:grid-cols-[1.1fr_0.9fr] gap-16 items-center">
        <motion.div
          initial={{ opacity: 0, y: 14 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6, ease: [0.16, 1, 0.3, 1] }}
        >
          <motion.div
            initial={{ opacity: 0, y: 10 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.5 }}
            className="inline-flex items-center gap-2.5 rounded-full hairline bg-charcoal/60 pl-1.5 pr-3.5 py-1 mb-7"
          >
            <span className="rounded-full bg-paper px-2 py-0.5 text-[10px] font-semibold tracking-wide text-ink">
              Nouveau
            </span>
            <span className="text-[12px] text-mist">
              Session Rewind arrive avec la v0.1.14
            </span>
          </motion.div>

          <h1 className="text-[40px] sm:text-[52px] md:text-[60px] leading-[1.04] tracking-tight font-medium text-paper">
            Un seul poste de
            <br />
            contrôle pour tous
            <br />
            tes <span className="underline decoration-[var(--hairline-strong)] decoration-2 underline-offset-4">agents de code</span>.
          </h1>

          <p className="mt-6 max-w-lg text-[16px] leading-relaxed text-mist">
            Komet pilote les CLI d&apos;agents que tu utilises déjà —
            sessions, transcripts, activité des outils et checkpoints, dans
            une seule fenêtre native en graphite, entièrement sur ta machine.
            100% local par défaut.
          </p>

          <div className="mt-9 flex flex-wrap items-center gap-3">
            <a
              href="#downloads"
              className="rounded-[var(--radius-control)] bg-paper px-5 py-2.5 text-[13.5px] font-medium text-ink transition-transform hover:-translate-y-0.5"
            >
              Télécharger komet
            </a>
            <a
              href="https://github.com/jomvick/komet"
              target="_blank"
              rel="noreferrer"
              className="rounded-[var(--radius-control)] hairline px-5 py-2.5 text-[13.5px] font-medium text-paper transition-colors hover:bg-graphite/50"
            >
              Voir le code source
            </a>
          </div>

          <p className="mt-5 font-mono text-[12px] text-fog">
            curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
          </p>

          <div className="mt-14">
            <AgentRow />
          </div>
        </motion.div>

        <motion.div
          initial={{ opacity: 0, y: 14 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6, delay: 0.1, ease: [0.16, 1, 0.3, 1] }}
          className="flex justify-center lg:justify-end"
        >
          <SessionCard />
        </motion.div>
      </div>
    </section>
  );
}
