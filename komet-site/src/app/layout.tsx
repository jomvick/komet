import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import "./globals.css";

export const metadata: Metadata = {
  title: "komet — one control room for all your coding agents",
  description:
    "Komet drives the agent CLIs you already use — sessions, transcripts, tool activity and checkpoints, in a single graphite native window, entirely on your machine. 100% local by default.",
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
