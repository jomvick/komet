"use client";

import { useEffect, useRef, useState } from "react";
import { assetUrl, useLatestRelease } from "@/lib/useLatestRelease";

type Entry = { label: string; asset: string };

/** Preferred user-facing asset per platform, in display order. */
const PREFERRED: Entry[] = [
  { label: "macOS (Apple Silicon)", asset: "macos-arm64.dmg" },
  { label: "Linux — tarball (x86_64)", asset: "linux-x86_64.tar.gz" },
  { label: "Linux — tarball (arm64)", asset: "linux-aarch64.tar.gz" },
  { label: "Linux — Debian/Ubuntu (.deb)", asset: "_amd64.deb" },
  { label: "Linux — Fedora/RHEL (.rpm)", asset: "x86_64.rpm" },
  { label: "Linux — AppImage", asset: "linux-x86_64.AppImage" },
  { label: "Windows (x86_64)", asset: "windows-x86_64.exe" },
];

function DownloadIcon() {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M12 15V3" />
      <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
      <path d="m7 10 5 5 5-5" />
    </svg>
  );
}

export default function DownloadButton({ label = "Download" }: { label?: string }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const release = useLatestRelease();

  useEffect(() => {
    function onClickOutside(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    function onEscape(e: KeyboardEvent) {
      if (e.key === "Escape") setOpen(false);
    }
    document.addEventListener("mousedown", onClickOutside);
    document.addEventListener("keydown", onEscape);
    return () => {
      document.removeEventListener("mousedown", onClickOutside);
      document.removeEventListener("keydown", onEscape);
    };
  }, []);

  // Only offer assets that actually exist in the latest release.
  // Assets are named komet-<version>-<platform>, so match on suffix.
  const entries = release
    ? PREFERRED.map((e) => ({
        ...e,
        fullName: release.assets.find((name) => name.endsWith(e.asset)),
      })).filter((e): e is Entry & { fullName: string } => Boolean(e.fullName))
    : [];

  return (
    <div ref={ref} className="relative inline-block">
      <button
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-haspopup="menu"
        className="inline-flex items-center gap-1.5 rounded-lg bg-primary text-primary-foreground px-4 h-10 text-sm font-medium hover:opacity-80 transition-opacity"
      >
        <DownloadIcon />
        {label}
      </button>

      {open && (
        <div
          role="menu"
          className="absolute left-0 top-full z-50 mt-2 w-56 rounded-lg bg-charcoal shadow-xl shadow-black/40 py-1.5"
        >
          {!release && (
            <a
              href="https://github.com/jomvick/komet/releases/latest"
              role="menuitem"
              onClick={() => setOpen(false)}
              className="block px-4 py-2 text-sm text-foreground hover:bg-foreground/10 transition-colors"
            >
              Browse versions on GitHub…
            </a>
          )}
          {release && entries.length === 0 && (
            <a
              href="https://github.com/jomvick/komet/releases/latest"
              role="menuitem"
              onClick={() => setOpen(false)}
              className="block px-4 py-2 text-sm text-foreground hover:bg-foreground/10 transition-colors"
            >
              No asset found — see GitHub…
            </a>
          )}
          {entries.map((e) => (
            <a
              key={e.asset}
              href={assetUrl(release!.version, e.fullName)}
              role="menuitem"
              onClick={() => setOpen(false)}
              className="block px-4 py-2 text-sm text-foreground hover:bg-foreground/10 transition-colors"
            >
              {e.label}
            </a>
          ))}
        </div>
      )}
    </div>
  );
}
