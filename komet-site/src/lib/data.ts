export const agents = [
  "Claude Code",
  "Codex",
  "Cursor",
  "Grok",
  "Hermes",
  "OpenCode",
  "Pi",
];

export const features = [
  {
    tag: "Engine",
    title: "Rust + gpui, a single binary",
    body: "The same engine that powers Zed. Instant launch, smooth scrolling even across years of transcripts, no Electron window to warm up.",
    icon: `<path d="M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z"/>`,
  },
  {
    tag: "Multi-agent",
    title: "Every agent, one timeline",
    body: "Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi — each wired through its strongest native interface, normalized into a single session model.",
    icon: `<path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z"/><path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12"/><path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17"/>`,
  },
  {
    tag: "History",
    title: "Rewind that keeps its promise",
    body: "Every prompt drops a checkpoint on your worktree through a hidden git ref. Step back through the code and the conversation — not just the chat.",
    icon: `<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>`,
  },
  {
    tag: "Keyboard",
    title: "Built to never leave the keyboard",
    body: "⌘N opens a session, ⏎ queues the next step while the agent works, ⌘⏎ steers mid-turn, Esc stops. No action ever requires the mouse.",
    icon: `<path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3"/>`,
  },
  {
    tag: "Local-first",
    title: "Local by architecture, not by option",
    body: "Projects, sessions, transcripts and credentials stay on disk. No account required, no telemetry, no cloud between you and your agents.",
    icon: `<path d="M10 16h.01"/><path d="M2.212 11.577a2 2 0 0 0-.212.896V18a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-5.527a2 2 0 0 0-.212-.896L18.55 5.11A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/><path d="M21.946 12.013H2.054"/><path d="M6 16h.01"/>`,
  },
  {
    tag: "Sync (optional)",
    title: "Multi-device when you decide",
    body: "Self-hosted sync ships built in — Loro CRDT, per-device relay, your own server — but stays off by default. A VPS can keep your agents running while your laptop goes offline.",
    icon: `<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>`,
  },
];

export const stats = [
  { label: "Startup", value: "< 100ms", note: "native binary, no VM to warm up" },
  { label: "Scrolling", value: "60fps", note: "years of transcripts, without a hitch" },
  { label: "Network by default", value: "0 calls", note: "strict local mode unless sync is on" },
  { label: "Binary", value: "just 1", note: "engine + interface, headed or headless" },
];

export const faq = [
  {
    q: "Is Komet yet another Electron client?",
    a: "No. The interface runs on gpui — the GPU-accelerated framework behind Zed — and the engine is a pure Rust daemon that works both headed (window) and headless on a server.",
  },
  {
    q: "Do I need new API keys?",
    a: "No. Komet drives the agent CLIs you already have installed and connected — it doesn't replace your access, it orchestrates it.",
  },
  {
    q: "Where does my data live?",
    a: "On your machine. Every device runs a small engine that stores its sessions locally. A fresh install starts in local-only mode, with no account and no network connection.",
  },
  {
    q: "What about multi-device sync?",
    a: "Shipped and off by default. Komet syncs through Loro CRDT across your own server (a VPS or a Cloudflare Worker is enough) — you opt in explicitly with komet sync-init, never automatically.",
  },
];

export const platforms = [
  {
    os: "Linux",
    detail: "Automatic install script, tarball, AppImage, .deb or .rpm package.",
    command: "curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh",
  },
  {
    os: "macOS",
    detail: "Disk image (.dmg, Apple Silicon) or launchd service via the CLI.",
    command: "komet daemon install",
  },
  {
    os: "Windows",
    detail: "Standalone executable (.exe) or ready-to-use .zip archive.",
    command: "komet.exe",
  },
];
