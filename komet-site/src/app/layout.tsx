import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import "./globals.css";

export const metadata: Metadata = {
  title: "komet — un seul poste de contrôle pour tous tes agents de code",
  description:
    "Komet pilote les CLI d'agents que tu utilises déjà — sessions, transcripts, activité des outils et checkpoints, dans une seule fenêtre native en graphite, entièrement sur ta machine. 100% local par défaut.",
};

export const viewport: Viewport = {
  themeColor: "#1e1e1e",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html
      lang="fr"
      className={`${GeistSans.variable} ${GeistMono.variable} min-h-dvh antialiased`}
    >
      <body className="bg-background text-foreground">
        <div className="mx-auto w-full max-w-[1100px]">
          {children}
        </div>
      </body>
    </html>
  );
}
