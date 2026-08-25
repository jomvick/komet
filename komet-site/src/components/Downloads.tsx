import { platforms } from "@/lib/data";

export default function Downloads() {
  return (
    <section id="downloads" className="px-5 py-16 md:px-10 scroll-mt-16">
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
        Installation
      </div>
      <h2 className="mt-3 text-[26px] md:text-[30px] font-medium tracking-tight text-balance">
        One command per platform.
      </h2>

      <div className="mt-8 grid grid-cols-1 gap-4 md:grid-cols-3">
        {platforms.map((p) => (
          <div
            key={p.os}
            className="rounded-xl border border-border bg-charcoal/30 p-6"
          >
            <h3 className="text-sm font-medium">{p.os}</h3>
            <p className="mt-1.5 text-[13px] leading-relaxed text-muted-foreground">
              {p.detail}
            </p>
            <pre className="mt-4 overflow-x-auto rounded-lg bg-background/60 border border-border px-3 py-2.5 font-mono text-[12px] text-foreground/90">
              {p.command}
            </pre>
          </div>
        ))}
      </div>

      <p className="mt-5 text-[13px] text-muted-foreground">
        All binaries (tarballs, AppImage, .dmg, .exe) are on{" "}
        <a
          href="https://github.com/jomvick/komet/releases/latest"
          target="_blank"
          rel="noreferrer"
          className="underline decoration-border hover:text-foreground hover:decoration-foreground transition-colors"
        >
          GitHub Releases
        </a>
        .
      </p>
    </section>
  );
}
