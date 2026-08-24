import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import "./globals.css";

export const metadata: Metadata = {
  title: "komet — un seul poste de contrôle pour tous tes agents de code",
  description:
    "Komet pilote Claude Code, Codex, Cursor, Grok, Hermes, OpenCode et Pi depuis une seule fenêtre native, 100% locale par défaut, avec rewind git et sync optionnelle.",
};

export const viewport: Viewport = {
  themeColor: "#060606",
};

import BackgroundFX from "@/components/BackgroundFX";

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html
      lang="fr"
      className={`${GeistSans.variable} ${GeistMono.variable} h-full antialiased`}
    >
      <body className="min-h-full flex flex-col bg-ink text-paper">
        <BackgroundFX />
        {children}
      </body>
    </html>
  );
}
