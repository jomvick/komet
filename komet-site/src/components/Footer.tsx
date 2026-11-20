export default function Footer() {
  return (
    <footer className="flex items-center justify-between gap-4 px-5 py-10 text-xs text-muted-foreground md:px-10">
      <div className="flex items-center gap-2">
        <img
          src="/komet.png"
          alt=""
          className="size-4 rounded-[4px] opacity-80 grayscale object-contain"
        />
        <span>© {new Date().getFullYear()} komet</span>
      </div>
      <a
        href="https://github.com/zeronsh/comet"
        target="_blank"
        rel="noreferrer"
        className="transition-colors hover:text-foreground"
      >
        Fork of Zeron ↗
      </a>
    </footer>
  );
}
