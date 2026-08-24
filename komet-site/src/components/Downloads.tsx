"use client";

import { useState } from "react";

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={() => {
        navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
      className="shrink-0 font-mono text-[11px] text-muted-foreground hover:text-foreground transition-colors"
    >
      {copied ? "copié ✓" : "copier"}
    </button>
  );
}

const INSTALL_CMD = "curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh";

export default function Downloads() {
  return (
    <section id="downloads" className="border-t px-5 py-16 md:px-10 md:py-20">
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
        Installer
      </div>
      <h2 className="mt-3 text-2xl font-semibold tracking-tight">
        Obtenir komet
      </h2>

      <div className="mt-6 flex flex-wrap items-center gap-x-5 gap-y-3">
        <a
          href="https://github.com/jomvick/komet/releases"
          target="_blank"
          rel="noreferrer"
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
          Télécharger
        </a>
        <span className="font-mono text-sm text-muted-foreground">v0.1.14</span>
      </div>

      <div className="mt-8 flex items-center justify-between gap-3 rounded-lg border border-border bg-charcoal/30 px-4 py-3 max-w-xl">
        <code className="font-mono text-[12px] text-foreground truncate">
          {INSTALL_CMD}
        </code>
        <CopyButton text={INSTALL_CMD} />
      </div>

      <p className="mt-5 text-sm text-muted-foreground">
        Les binaires signés et notariés se mettent à jour automatiquement.
        Consulte le{" "}
        <a
          href="https://github.com/jomvick/komet/releases"
          target="_blank"
          rel="noreferrer"
          className="text-foreground underline decoration-border hover:decoration-foreground transition-colors"
        >
          dépôt des releases
        </a>{" "}
        pour les installeurs Windows et macOS.
      </p>
    </section>
  );
}

