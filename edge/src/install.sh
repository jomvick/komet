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

BASE="${KOMET_BASE_URL:-https://github.com/jomvick/komet/releases/download/v1.0.0}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "komet install: on macOS, download the desktop app instead:" >&2
    echo "  $BASE/komet-1.0.0-macos-arm64.dmg" >&2
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

# --- download ----------------------------------------------------------------
file="komet-1.0.0-$plat-$arch.tar.gz"
data_root="$HOME/.komet"
app_root="$data_root/app"
dest="$app_root/1.0.0"

if [ -x "$dest/komet" ]; then
  echo "komet 1.0.0 already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading komet 1.0.0 ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/$file" -o "$tmp/$file"
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
echo "✓ komet 1.0.0 installed$path_hint"
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
