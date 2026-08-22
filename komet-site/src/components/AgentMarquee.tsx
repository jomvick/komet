import { agents } from "@/lib/data";

export default function AgentMarquee() {
  const doubled = [...agents, ...agents];

  return (
    <section className="border-y border-[var(--hairline)] bg-charcoal/40 py-6">
      <div className="mx-auto max-w-6xl px-5">
        <p className="font-mono text-[11px] uppercase tracking-wide text-fog mb-4">
          Pilote les agents que tu as déjà
        </p>
      </div>
      <div className="relative overflow-hidden">
        <div className="pointer-events-none absolute inset-y-0 left-0 w-24 bg-gradient-to-r from-ink to-transparent z-10" />
        <div className="pointer-events-none absolute inset-y-0 right-0 w-24 bg-gradient-to-l from-ink to-transparent z-10" />
        <div className="marquee flex w-max gap-3">
          {doubled.map((agent, i) => (
            <span
              key={i}
              className="hairline rounded-[var(--radius-control)] bg-ash px-4 py-2 text-[13px] text-mist whitespace-nowrap"
            >
              {agent}
            </span>
          ))}
        </div>
      </div>
    </section>
  );
}
