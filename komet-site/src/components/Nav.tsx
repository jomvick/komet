"use client";

import { motion } from "framer-motion";

export default function Nav() {
  return (
    <motion.header
      initial={{ y: -24, opacity: 0 }}
      animate={{ y: 0, opacity: 1 }}
      transition={{ duration: 0.6, ease: [0.16, 1, 0.3, 1] }}
      className="fixed top-0 inset-x-0 z-50"
    >
      <div className="mx-auto max-w-6xl px-5 pt-4">
        <div className="glass hairline flex items-center justify-between rounded-[var(--radius-panel)] px-4 py-2.5 shadow-[0_1px_0_rgba(255,255,255,0.04)_inset]">
          <a href="#" className="flex items-center gap-2.5 shrink-0">
            <span className="relative flex h-6 w-6 items-center justify-center rounded-[7px] bg-graphite hairline">
              <span className="h-2 w-2 rounded-full bg-signal shadow-[0_0_10px_var(--signal)]" />
            </span>
            <span className="text-[14px] font-medium tracking-tight text-paper">
              komet
            </span>
          </a>

          <nav className="hidden md:flex items-center gap-6 text-[13px] text-mist">
            <a href="#features" className="hover:text-paper transition-colors">
              Fonctionnalités
            </a>
            <a href="#speed" className="hover:text-paper transition-colors">
              Performance
            </a>
            <a href="#downloads" className="hover:text-paper transition-colors">
              Installer
            </a>
            <a href="#faq" className="hover:text-paper transition-colors">
              Questions
            </a>
            <a
              href="https://github.com/opencode/komet"
              target="_blank"
              rel="noreferrer"
              className="hover:text-paper transition-colors"
            >
              GitHub ↗
            </a>
          </nav>

          <a
            href="#downloads"
            className="shrink-0 rounded-[var(--radius-control)] bg-paper px-3.5 py-1.5 text-[13px] font-medium text-ink transition-opacity hover:opacity-85"
          >
            Télécharger
          </a>
        </div>
      </div>
    </motion.header>
  );
}
