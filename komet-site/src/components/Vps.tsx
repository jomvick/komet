import CopyCommand from "./CopyCommand";

const steps: {
  title: string;
  body: string;
  commands?: { label?: string; command: string }[];
}[] = [
  {
    title: "Prepare the box",
    body: "Any always-on Linux machine. The binary needs gpui's shared libs even without a window — no display server — plus the agent CLIs you want to run there.",
    commands: [
      {
        label: "Debian / Ubuntu",
        command:
          "sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 libdbus-1-3",
      },
      {
        label: "Fedora",
        command: "sudo dnf install libxcb libxkbcommon libxkbcommon-x11 dbus-libs",
      },
    ],
  },
  {
    title: "Install it",
    body: "One command installs the binary, generates the token in ~/.komet/serve.json, starts the systemd user service and prints a komet://pair/… link. --lan serves HTTP on your private network.",
    commands: [
      {
        command:
          "curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh -s -- --serve --lan",
      },
    ],
  },
  {
    title: "Keep it alive",
    body: "Without lingering the service stops with your session. On a home PC, also disable suspend — sessions die with the machine.",
    commands: [{ command: "loginctl enable-linger $USER" }],
  },
  {
    title: "Add TLS",
    body: "Only if the desktop is not on the same LAN. Point a domain at the box, proxy to :8787, then save the public URL — or skip DNS entirely and pass the https URL a tunnel prints (instabox port publish, cloudflared). Skip on a trusted LAN.",
    commands: [
      {
        command:
          "komet serve --public-url https://sync.your-domain.com --install-service",
      },
    ],
  },
  {
    title: "Pair the desktop",
    body: "Settings → Sync → Add device, paste the link. It is a secret: it grants a shell on that host. Lost it? komet serve --print-link.",
  },
  {
    title: "Use and manage",
    body: "Pick the machine in the Devices menu at the top of the sidebar: sessions run there, and Settings → Providers targets its agents. Restart after editing serve.json.",
    commands: [{ command: "komet daemon status\nkomet daemon restart" }],
  },
];

export default function Vps() {
  return (
    <section id="vps" className="px-5 py-16 md:px-10 scroll-mt-16">
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
        Self-hosted
      </div>
      <h2 className="mt-3 text-[26px] md:text-[30px] font-medium tracking-tight text-balance">
        Your own VPS, in six cards.
      </h2>
      <p className="mt-3 max-w-2xl text-[14px] leading-relaxed text-muted-foreground">
        komet serve runs the sync server and the engine in one process. Your
        desktop stays the controller — it never becomes a device in that
        workspace.
      </p>

      <ol className="mt-8 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {steps.map((step, i) => (
          <li
            key={step.title}
            className="flex flex-col rounded-xl border border-border bg-charcoal/30 p-5"
          >
            <div className="flex items-baseline gap-3">
              <span className="font-mono text-[11px] tabular-nums text-muted-foreground/50">
                {String(i + 1).padStart(2, "0")}
              </span>
              <h3 className="text-sm font-medium">{step.title}</h3>
            </div>
            <p className="mt-2 text-[13px] leading-relaxed text-muted-foreground">
              {step.body}
            </p>
            {step.commands && (
              <div className="mt-auto">
                {step.commands.map((cmd) => (
                  <CopyCommand
                    key={cmd.command}
                    command={cmd.command}
                    label={cmd.label}
                    wrap
                  />
                ))}
              </div>
            )}
          </li>
        ))}
      </ol>

      <p className="mt-5 text-[13px] text-muted-foreground">
        Full reference, Fedora libs, Caddy config and teardown:{" "}
        <a
          href="https://github.com/jomvick/komet/blob/main/docs/environments.md"
          target="_blank"
          rel="noreferrer"
          className="underline decoration-border hover:text-foreground hover:decoration-foreground transition-colors"
        >
          docs/environments.md
        </a>
        .
      </p>
    </section>
  );
}
