"use client";

import AgentRow from "./AgentRow";

export default function Hero() {
  return (
    <section className="px-5 pt-14 pb-14 md:px-10 md:pt-24">
      {/* Badge */}
      <div className="mb-7 inline-flex items-center gap-2 rounded-full border border-border px-3 py-1 text-xs text-muted-foreground">
        <span className="flex size-3.5 items-center justify-center rounded-[3px] bg-foreground text-[10px] font-bold text-background">
          N
        </span>
        Session Rewind arrive avec la v0.1.14
      </div>

      {/* Headline */}
      <h1 className="max-w-4xl text-4xl font-semibold tracking-[-0.03em] text-balance md:text-[3.4rem] md:leading-[1.04]">
        Un seul poste de contrôle pour tous tes agents de code.
      </h1>

      {/* Sub */}
      <p className="mt-5 max-w-[36rem] text-[17px] leading-relaxed text-pretty text-muted-foreground">
        Komet pilote les CLI d&apos;agents que tu utilises déjà — sessions,
        transcripts, activité des outils et checkpoints, dans une seule fenêtre
        native en graphite, entièrement sur ta machine.
      </p>

      {/* CTA */}
      <div className="mt-8 flex flex-wrap items-center gap-x-5 gap-y-3">
        <a
          href="#downloads"
          className="inline-flex items-center gap-1.5 rounded-lg bg-primary text-primary-foreground px-4 h-10 text-sm font-medium hover:opacity-80 transition-opacity"
        >
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path d="M12 15V3" />
            <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
            <path d="m7 10 5 5 5-5" />
          </svg>
          Télécharger komet
        </a>
        <span className="font-mono text-[13px] text-muted-foreground">
          v0.1.14
        </span>
      </div>

      {/* Install command */}
      <p className="mt-5 font-mono text-[12px] text-muted-foreground/70">
        curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
      </p>

      {/* Agents */}
      <div className="mt-16">
        <AgentRow />
      </div>
    </section>
  );
}

