interface FooterLink {
  label: string;
  href: string;
  external?: boolean;
}

interface FooterColumn {
  title: string;
  links: FooterLink[];
}

const columns: FooterColumn[] = [
  {
    title: "Produit",
    links: [
      { label: "Fonctionnalités", href: "#features" },
      { label: "Performance", href: "#speed" },
      { label: "Installer", href: "#downloads" },
      { label: "Questions", href: "#faq" },
    ],
  },
  {
    title: "Développeurs",
    links: [
      {
        label: "Code source",
        href: "https://github.com/jomvick/komet",
        external: true,
      },
      {
        label: "Releases",
        href: "https://github.com/jomvick/komet/releases",
        external: true,
      },
      {
        label: "Licence MIT",
        href: "https://github.com/jomvick/komet/blob/main/LICENSE",
        external: true,
      },
    ],
  },
];

export default function Footer() {
  return (
    <footer className="border-t border-[var(--hairline)] pt-16 pb-8">
      <div className="mx-auto max-w-6xl px-5">
        <div className="grid sm:grid-cols-[1.4fr_1fr_1fr] gap-10 pb-12">
          <div>
            <div className="flex items-center gap-2.5">
              <img
                src="/komet.png"
                alt="komet"
                className="h-7 w-7 object-contain"
              />
              <span className="text-[14px] font-medium tracking-tight text-paper">
                komet
              </span>
            </div>
            <p className="mt-3 max-w-xs text-[13px] leading-relaxed text-fog">
              Un seul poste de contrôle natif pour tous tes agents de code.
              Local par défaut, sync quand tu veux.
            </p>
          </div>

          {columns.map((col) => (
            <div key={col.title}>
              <p className="font-mono text-[11px] uppercase tracking-wide text-fog mb-4">
                {col.title}
              </p>
              <ul className="space-y-2.5">
                {col.links.map((link) => (
                  <li key={link.label}>
                    <a
                      href={link.href}
                      target={link.external ? "_blank" : undefined}
                      rel={link.external ? "noreferrer" : undefined}
                      className="text-[13.5px] text-mist hover:text-paper transition-colors"
                    >
                      {link.label}
                    </a>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>

        <div className="flex flex-col sm:flex-row items-center justify-between gap-3 border-t border-[var(--hairline)] pt-6">
          <span className="text-[12px] text-fog">
            © {new Date().getFullYear()} komet · projet open source
          </span>
          <span className="text-[12px] text-fog">Licence MIT</span>
        </div>
      </div>
    </footer>
  );
}
