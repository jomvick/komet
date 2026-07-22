#!/bin/sh
# Komet (native) headless installer.
#
#   curl -fsSL https://komet.komet.sh/install.sh | sh
#
# Installs the self-contained native binary (no runtime deps) to
# ~/.komet-native/app, puts `komet` on PATH, and — once you've signed in —
# runs it as a systemd user service that survives reboots. Re-running
# upgrades in place; ~/.komet-native state is preserved.
#
# The binary ships with production endpoints baked in: no KOMET_EDGE_URL or
# client-id configuration needed. Overrides (if any) go in ~/.komet-native/env.
set -eu

BASE="${KOMET_BASE_URL:-https://komet.komet.sh}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "komet install: on macOS, download the desktop app instead:" >&2
    echo "  $BASE/releases/latest.txt → $BASE/releases/komet-<version>-macos-arm64.dmg" >&2
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
ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "komet install: could not resolve latest version" >&2; exit 1; }
file="komet-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.komet-native"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/komet" ]; then
  echo "komet $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading komet $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/komet" "$HOME/.local/bin/komet"

# --- service -----------------------------------------------------------------
# The engine holds an exclusive data-dir lock, so the service starts only after
# first sign-in (`komet headless` prompts the paste-code flow on a TTY).
signed_in=no
[ -f "$data_root/session.json" ] && signed_in=yes

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/komet-native.service" <<'UNIT'
[Unit]
Description=Komet native headless engine
After=network-online.target

[Service]
ExecStart=%h/.komet-native/app/current/komet headless
Restart=always
RestartSec=2
EnvironmentFile=-%h/.komet-native/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable komet-native >/dev/null 2>&1 || true
  if [ "$signed_in" = yes ]; then
    systemctl --user restart komet-native
    service=running
  else
    service=ready
  fi
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
    echo "the engine restarted with the new version."
    echo "  systemctl --user status komet-native    check the service"
    ;;
  ready)
    echo "next steps:"
    echo "  komet headless                           first run prompts sign-in (paste-code)"
    echo "  then Ctrl-C and:  systemctl --user start komet-native"
    ;;
  manual)
    echo "next: run \`komet headless\` — the first run prompts sign-in."
    ;;
esac
