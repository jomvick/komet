import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { SITE_URL } from "@/lib/site";
import "./globals.css";

const title = "komet — one control room for all your coding agents";
const description =
  "Komet drives the agent CLIs you already use — sessions, transcripts, tool activity and checkpoints, in a single graphite native window, entirely on your machine. 100% local by default.";

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title,
  description,
  openGraph: {
    title,
    description,
    url: SITE_URL,
    siteName: "komet",
    type: "website",
    locale: "en_US",
  },
  twitter: {
    card: "summary_large_image",
    title,
    description,
  },
};

export const viewport: Viewport = {
  themeColor: "#1e1e1e",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html
      lang="en"
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
