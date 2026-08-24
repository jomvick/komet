import { features } from "@/lib/data";

export default function Features() {
  return (
    <section>
      <div className="px-5 pt-14 md:px-10">
        <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
          Pourquoi natif
        </div>
      </div>

      <div className="mt-8 grid grid-cols-1 gap-px sm:grid-cols-2 lg:grid-cols-3">
        {features.map((f) => (
          <div key={f.title} className="bg-background p-6 md:p-8">
            <div className="flex items-center gap-2.5">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="24"
                height="24"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                className="size-4 text-muted-foreground"
                aria-hidden="true"
                dangerouslySetInnerHTML={{ __html: f.icon }}
              >
              </svg>
              <h3 className="text-sm font-medium">{f.title}</h3>
            </div>
            <p className="mt-2.5 text-sm leading-relaxed text-muted-foreground">
              {f.body}
            </p>
          </div>
        ))}
      </div>
    </section>
  );
}

