"use client";

import { useState } from "react";
import { Check, Copy } from "lucide-react";

export default function CopyCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      // Clipboard API unavailable (non-secure context) — select fallback not needed for a <pre>
    }
  }

  return (
    <div className="group relative mt-4">
      <pre className="overflow-x-auto rounded-lg bg-background/60 border border-border px-3 py-2.5 pr-10 font-mono text-[12px] text-foreground/90">
        {command}
      </pre>
      <button
        onClick={copy}
        aria-label={copied ? "Copied" : "Copy command"}
        className="absolute right-2 top-1/2 -translate-y-1/2 rounded-md p-1.5 text-muted-foreground opacity-0 transition-opacity hover:bg-foreground/10 hover:text-foreground focus-visible:opacity-100 group-hover:opacity-100"
      >
        {copied ? <Check size={14} /> : <Copy size={14} />}
      </button>
    </div>
  );
}
