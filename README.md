# Komet

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) locally by default, with optional multi-device sync.

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

## Install from source (Linux)

### 1. Install dependencies

```bash
# Fedora / RHEL
sudo dnf install gcc g++ pkg-config wayland-devel libX11-devel libxkbcommon-devel fontconfig-devel

# Ubuntu / Debian
sudo apt install gcc g++ pkg-config libwayland-dev libx11-dev libxkbcommon-dev libfontconfig-dev

# Arch
sudo pacman -S gcc pkg-config wayland libx11 libxkbcommon fontconfig
```

You also need the [Rust stable toolchain](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Build and install

```bash
git clone https://github.com/jomvick/komet.git
cd komet
cargo build --release -p komet

# Install to ~/.local/bin
mkdir -p ~/.local/bin
cp target/release/komet ~/.local/bin/komet
```

Make sure `~/.local/bin` is on your `PATH`.

### 3. Run

```bash
komet            # headed mode (opens the GUI)
komet headless   # engine only (for remote control from another device)
```

The engine runs locally-first by default — no account, no network calls.

Day-to-day:

```bash
komet status      # local/synced mode and engine status
komet daemon install   # install as a systemd user service (Linux)
komet daemon start|stop|restart|status
```


## Install on macOS

Build from source following the same steps above (Xcode command-line tools provide the C toolchain; install Wayland/X11 deps via Homebrew if needed, or use the desktop app bundle when available).

Alternatively, install as a launchd service:

```bash
komet daemon install
```

## Install on Windows

Build from source with the Rust toolchain installed:

```powershell
git clone https://github.com/jomvick/komet.git
cd komet
cargo build --release -p komet
```

Or download the latest `.msi` installer from the [GitHub release](https://github.com/jomvick/komet/releases) and run it — it installs `komet.exe` into `Program Files\Komet`, adds it to `PATH` and registers uninstall keys. A standalone `komet.exe` portable executable is also available in the release assets.

The `Komet` Windows service (running as the logged-in user) is not registered by the installer; add it with:

```powershell
komet daemon install
```

or equivalently via `sc`:

```powershell
sc create Komet binPath= "\"c:\Program Files\Komet\komet.exe\" --service"
```

Day-to-day management:

```powershell
komet daemon start
komet daemon stop
komet daemon restart
komet daemon status
```

## Multi-device sync (future)

Komet currently runs **100% locally**: no account, no login screen, no network calls. Every session, attachment and diff stays on the machine that created it.

Multi-device sync is part of the app's roadmap. The codebase already contains the building blocks — the `edge/` sync worker (Loro CRDT rooms, device relays, R2 attachments) and the WorkOS auth routes — but they are disabled by default. To enable sync later:

1. Deploy the `edge/` worker to your own Cloudflare account and create a WorkOS AuthKit app (set the real client id in `edge/wrangler.jsonc`).
2. Run the engine with `KOMET_WORKOS_CLIENT_ID=<your client id>`, and `KOMET_EDGE_URL=<your edge host>` if you self-host instead of using the default edge endpoint.

With sync enabled you could start an agent on one device and follow or drive it from another; an always-on machine such as a VPS can keep those agents working after you close your laptop. Until then, `komet login` just reports "dev mode — there is nothing to sign in to" and the app never leaves the machine.

---

Developing or curious how it works? Check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
