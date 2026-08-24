export default function Footer() {
  return (
    <footer className="flex items-center gap-2 border-t px-5 py-10 text-xs text-muted-foreground md:px-10">
      <img
        src="/komet.png"
        alt=""
        className="size-4 rounded-[4px] opacity-80 grayscale object-contain"
      />
      <span>© {new Date().getFullYear()} komet</span>
    </footer>
  );
}

