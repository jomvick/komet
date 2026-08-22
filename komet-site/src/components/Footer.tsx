export default function Footer() {
  return (
    <footer className="border-t border-[var(--hairline)] py-10">
      <div className="mx-auto max-w-6xl px-5 flex flex-col sm:flex-row items-center justify-between gap-4">
        <div className="flex items-center gap-2.5">
          <span className="relative flex h-6 w-6 items-center justify-center rounded-[7px] bg-graphite hairline">
            <span className="h-2 w-2 rounded-full bg-signal" />
          </span>
          <span className="text-[13px] text-mist">
            komet · local par défaut, sync quand tu veux
          </span>
        </div>

        <div className="flex items-center gap-5 text-[13px] text-fog">
          <a
            href="https://github.com/opencode/komet"
            target="_blank"
            rel="noreferrer"
            className="hover:text-paper transition-colors"
          >
            GitHub
          </a>
          <a
            href="https://github.com/opencode/komet/releases"
            target="_blank"
            rel="noreferrer"
            className="hover:text-paper transition-colors"
          >
            Releases
          </a>
          <span>Licence MIT</span>
        </div>
      </div>
    </footer>
  );
}
