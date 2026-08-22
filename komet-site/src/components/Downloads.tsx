"use client";

import { useState } from "react";
import { motion } from "framer-motion";
import { platforms } from "@/lib/data";

const easing = [0.16, 1, 0.3, 1] as const;

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={() => {
        navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
      className="shrink-0 font-mono text-[11px] text-fog hover:text-paper transition-colors"
    >
      {copied ? "copié" : "copier"}
    </button>
  );
}

export default function Downloads() {
  return (
    <section id="downloads" className="py-24 md:py-32">
      <div className="mx-auto max-w-6xl px-5">
        <motion.div
          initial={{ opacity: 0, y: 16 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
          className="max-w-xl mb-14"
        >
          <p className="font-mono text-[11px] uppercase tracking-wide text-signal mb-3">
            Installer
          </p>
          <h2 className="text-[30px] md:text-[36px] leading-tight tracking-tight font-medium text-paper">
            Choisis ta plateforme. Le daemon démarre tout seul.
          </h2>
        </motion.div>

        <div className="grid md:grid-cols-3 gap-4">
          {platforms.map((p, i) => (
            <motion.div
              key={p.os}
              initial={{ opacity: 0, y: 20 }}
              whileInView={{ opacity: 1, y: 0 }}
              viewport={{ once: true, margin: "-60px" }}
              transition={{ duration: 0.55, delay: i * 0.1, ease: easing }}
              className="hairline rounded-[var(--radius-panel)] bg-ash/60 p-6 flex flex-col"
            >
              <h3 className="text-[16px] font-medium text-paper">{p.os}</h3>
              <p className="mt-2 text-[13px] leading-relaxed text-mist flex-1">
                {p.detail}
              </p>
              <div className="mt-5 flex items-center justify-between gap-3 rounded-[var(--radius-control)] bg-charcoal hairline px-3 py-2.5">
                <code className="font-mono text-[11.5px] text-violet truncate">
                  {p.command}
                </code>
                <CopyButton text={p.command} />
              </div>
            </motion.div>
          ))}
        </div>

        <motion.p
          initial={{ opacity: 0 }}
          whileInView={{ opacity: 1 }}
          viewport={{ once: true }}
          transition={{ duration: 0.6, delay: 0.2 }}
          className="mt-8 text-[13px] text-fog"
        >
          Les binaires signés et notariés se mettent à jour automatiquement.
          Consulte le{" "}
          <a
            href="https://github.com/opencode/komet/releases"
            target="_blank"
            rel="noreferrer"
            className="text-signal hover:underline"
          >
            dépôt des releases
          </a>{" "}
          pour les installeurs Windows et macOS.
        </motion.p>
      </div>
    </section>
  );
}
