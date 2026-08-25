"use client";

import AgentRow from "./AgentRow";
import DownloadButton from "./DownloadButton";
import { useLatestRelease } from "@/lib/useLatestRelease";

export default function Hero() {
  const release = useLatestRelease();
  return (
    <section className="px-5 pt-14 pb-14 md:px-10 md:pt-24">
      {/* Badge */}
      <div className="mb-7 inline-flex items-center gap-2 rounded-full border border-border px-3 py-1 text-xs text-muted-foreground">
        <span className="flex size-3.5 items-center justify-center rounded-[3px] bg-foreground text-[10px] font-bold text-background">
          N
        </span>
        {release ? `v${release.version} is out` : "Download"}
      </div>

      {/* Headline */}
      <h1 className="max-w-4xl text-4xl font-semibold tracking-[-0.03em] text-balance md:text-[3.4rem] md:leading-[1.04]">
        One control room for all your coding agents.
      </h1>

      {/* Sub */}
      <p className="mt-5 max-w-[36rem] text-[17px] leading-relaxed text-pretty text-muted-foreground">
        Komet drives the agent CLIs you already use — sessions,
        transcripts, tool activity and checkpoints, in a single graphite
        native window, entirely on your machine.
      </p>

      {/* CTA */}
      <div className="mt-8 flex flex-wrap items-center gap-x-5 gap-y-3">
        <DownloadButton label="Download komet" />
        <span className="font-mono text-[13px] text-muted-foreground">
          {release ? `v${release.version}` : ""}
        </span>
      </div>

      {/* All download options */}
      <p className="mt-5 text-[13px]">
        <a
          href="https://github.com/jomvick/komet/releases/latest"
          target="_blank"
          rel="noreferrer"
          className="text-muted-foreground underline decoration-border hover:text-foreground hover:decoration-foreground transition-colors"
        >
          All download options
        </a>
      </p>

      {/* Agents */}
      <div className="mt-16">
        <AgentRow />
      </div>

      {/* App screenshot framed like paseo.sh */}
      <div className="relative mx-auto mt-10 max-w-5xl overflow-hidden rounded-2xl border border-border">
        <img
          src="/deep-field.jpg"
          alt=""
          aria-hidden="true"
          className="absolute inset-0 h-full w-full object-cover"
        />
        <div className="absolute inset-0 bg-background/30" />
        <div className="relative flex justify-center px-6 py-10 md:px-12 md:py-14">
          <img
            src="/app-screenshot.png"
            alt="Komet app"
            className="w-full max-w-4xl rounded-xl border border-border shadow-2xl shadow-black/60"
          />
        </div>
      </div>
    </section>
  );
}

