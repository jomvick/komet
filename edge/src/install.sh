#!/bin/sh
# Komet (native) headless installer.
#
#   curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/edge/src/install.sh | sh
#
# Installs the self-contained native binary (no runtime deps) to
# ~/.komet/app, puts `komet` on PATH, and runs it as a local-only
# systemd user service that survives reboots. Signing in is optional and
# enables sync after a restart. Re-running
# upgrades in place; ~/.komet state is preserved.
#
# The binary ships with production endpoints baked in: no KOMET_EDGE_URL or
# client-id configuration needed. Overrides (if any) go in ~/.komet/env.
set -eu

GITHUB="https://github.com/jomvick/komet/releases/latest/download"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "komet install: on macOS, download the desktop app instead:" >&2
    echo "  $GITHUB/komet-macos-arm64.dmg" >&2
    exit 1
    ;;
  *)
    echo "komet install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "komet install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- resolve version ---------------------------------------------------------
# GitHub /releases/latest/download redirects to the latest tag's assets.
# We resolve the final URL to extract the version for directory naming.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

file="komet-$plat-$arch.tar.gz"
url="$GITHUB/$file"

echo "resolving latest komet version…"
http_code=$(curl -fsSL -o "$tmp/$file" -w "%{http_code}" "$url" 2>/dev/null) || true

if [ "$http_code" = "404" ] || [ ! -s "$tmp/$file" ]; then
  echo "komet install: no release asset found for $plat-$arch" >&2
  echo "  build from source instead — see README.md" >&2
  exit 1
fi

# Extract version from tarball filename inside (first entry)
ver="$(tar -tzf "$tmp/$file" 2>/dev/null | head -1 | sed 's|komet-\([^/]*\)/.*|\1|' || echo "latest")"

data_root="$HOME/.komet"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/komet" ]; then
  echo "komet $ver already downloaded — relinking."
else
  echo "installing komet $ver ($plat-$arch)…"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/komet" "$HOME/.local/bin/komet"

# --- service -----------------------------------------------------------------
# The daemon is useful before auth: without a saved session it serves the local
# profile. Login only changes which profile the next daemon start selects.

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/komet.service" <<'UNIT'
[Unit]
Description=Komet native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.komet/app/current/komet headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.komet/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable komet
  systemctl --user restart komet
  service=running
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: komet headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI not found — install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "✓ komet $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine is running with the new version (local-only unless sync is enabled)."
    echo "  systemctl --user status komet    check the service"
    echo ""
    echo "optional sync (local sessions stay local):"
    echo "  systemctl --user stop komet"
    echo "  komet login"
    echo "  systemctl --user restart komet"
    ;;
  manual)
    echo "next: run the local-only engine with \`komet headless\`."
    echo "optional sync: run \`komet login\` before starting the engine."
    ;;
esac
