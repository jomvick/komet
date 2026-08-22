"use client";

import { motion } from "framer-motion";

const lines = [
  {
    who: "you",
    text: "refactor the auth middleware to use the new session store",
    color: "text-paper",
  },
  {
    who: "claude-code",
    text: "Reading crates/engine/src/auth.rs and 3 related files…",
    color: "text-signal",
  },
  {
    who: "claude-code",
    text: "Applying edits · 2 files changed, checkpoint 4f9a1c",
    color: "text-signal",
  },
];

const dots = [
  { label: "1a2f", active: false },
  { label: "88de", active: false },
  { label: "4f9a", active: true },
  { label: "now", active: false, ghost: true },
];

export default function SessionCard() {
  return (
    <motion.div
      initial={{ opacity: 0, y: 28, scale: 0.98 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      transition={{ duration: 0.8, delay: 0.25, ease: [0.16, 1, 0.3, 1] }}
      className="relative w-full max-w-md"
    >
      {/* ambient glow */}
      <div className="drift absolute -inset-10 -z-10 rounded-full bg-signal/20 blur-[80px]" />

      <div className="glass hairline rounded-[var(--radius-panel)] overflow-hidden shadow-[0_30px_80px_-20px_rgba(0,0,0,0.7)]">
        {/* titlebar */}
        <div className="flex items-center justify-between px-4 py-2.5 border-b border-[var(--hairline)] bg-charcoal/60">
          <div className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-full bg-ember/70" />
            <span className="h-2.5 w-2.5 rounded-full bg-amber/70" />
            <span className="h-2.5 w-2.5 rounded-full bg-moss/70" />
          </div>
          <span className="font-mono text-[11px] text-fog">auth-refactor · local</span>
          <span className="flex items-center gap-1.5 font-mono text-[11px] text-moss">
            <span className="h-1.5 w-1.5 rounded-full bg-moss" />
            live
          </span>
        </div>

        {/* transcript */}
        <div className="px-4 py-4 space-y-3 min-h-[168px]">
          {lines.map((line, i) => (
            <motion.div
              key={i}
              initial={{ opacity: 0, x: -8 }}
              animate={{ opacity: 1, x: 0 }}
              transition={{ delay: 0.9 + i * 0.55, duration: 0.4, ease: "easeOut" }}
              className="flex items-start gap-2.5"
            >
              <span className="mt-[3px] h-4 w-4 shrink-0 rounded-[4px] bg-graphite hairline flex items-center justify-center">
                <span
                  className={`h-1.5 w-1.5 rounded-full ${
                    line.who === "you" ? "bg-mist" : "bg-signal"
                  }`}
                />
              </span>
              <p className="text-[12.5px] leading-relaxed text-mist font-mono">
                <span className={`${line.color} font-medium`}>{line.who}</span>
                <span className="text-fog"> · </span>
                {line.text}
              </p>
            </motion.div>
          ))}

          {/* thinking orbs */}
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ delay: 2.4, duration: 0.4 }}
            className="flex items-center gap-1.5 pl-[26px]"
          >
            {[0, 1, 2].map((i) => (
              <motion.span
                key={i}
                animate={{ opacity: [0.25, 1, 0.25], y: [0, -3, 0] }}
                transition={{
                  duration: 1.1,
                  repeat: Infinity,
                  delay: i * 0.18,
                  ease: "easeInOut",
                }}
                className="h-1.5 w-1.5 rounded-full bg-busy"
              />
            ))}
            <span className="font-mono text-[11px] text-fog ml-1">
              working<span className="caret-blink">_</span>
            </span>
          </motion.div>
        </div>

        {/* rewind scrubber */}
        <div className="border-t border-[var(--hairline)] bg-charcoal/60 px-4 py-3">
          <div className="flex items-center justify-between mb-2">
            <span className="font-mono text-[10.5px] uppercase tracking-wide text-fog">
              rewind
            </span>
            <span className="font-mono text-[10.5px] text-fog">4 checkpoints</span>
          </div>
          <div className="relative h-[3px] rounded-full bg-graphite">
            <motion.div
              initial={{ width: "0%" }}
              animate={{ width: "78%" }}
              transition={{ delay: 1, duration: 1.4, ease: "easeInOut" }}
              className="absolute inset-y-0 left-0 rounded-full bg-signal/70"
            />
            {dots.map((d, i) => (
              <span
                key={i}
                style={{ left: `${(i / (dots.length - 1)) * 100}%` }}
                className="absolute -top-[3.5px] -translate-x-1/2"
              >
                <span
                  className={`block h-2.5 w-2.5 rounded-full hairline ${
                    d.active
                      ? "bg-signal shadow-[0_0_8px_var(--signal)]"
                      : d.ghost
                        ? "bg-transparent"
                        : "bg-graphite"
                  }`}
                />
              </span>
            ))}
          </div>
        </div>
      </div>
    </motion.div>
  );
}
