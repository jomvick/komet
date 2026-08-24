export default function Nav() {
  return (
    <header className="relative border-b border-[var(--hairline)]">
      <div className="mx-auto max-w-6xl px-5 py-4 flex items-center justify-between">
        <a href="#" className="flex items-center gap-2.5 shrink-0">
          <img src="/komet.png" alt="komet" className="h-8 w-8 object-contain" />
          <span className="text-[14px] font-medium tracking-tight text-paper">komet</span>
        </a>

        <div className="flex items-center gap-3 shrink-0">
          <a
            href="https://github.com/jomvick/komet"
            target="_blank"
            rel="noreferrer"
            className="text-[13px] text-mist hover:text-paper transition-colors"
          >
            GitHub ↗
          </a>
          <a
            href="#downloads"
            className="rounded-[var(--radius-control)] bg-paper px-3.5 py-1.5 text-[13px] font-medium text-ink transition-opacity hover:opacity-85"
          >
            Télécharger
          </a>
        </div>
      </div>
    </header>
  );
}
