#!/usr/bin/env sh
# Install Komet (Linux) from GitHub Releases — no root needed.
#
#   curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
#
# Installs the CLI into ~/.komet/app/<ver> with a `current` symlink (the layout
# `komet update` manages), links the binary into ~/.local/bin, and drops the
# .desktop entry + icon into ~/.local/share.
set -eu

REPO="${KOMET_RELEASE_REPO:-jomvick/komet}"
PREFIX="$HOME/.komet"
BIN_DIR="$HOME/.local/bin"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 \
  || die "curl or wget is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

fetch() { # fetch <url> <outfile>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  else
    wget -qO "$2" "$1"
  fi
}

# --- platform ----------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS:$ARCH" in
  Linux:x86_64)  PLATFORM="linux-x86_64" ;;
  Linux:aarch64|Linux:arm64) PLATFORM="linux-aarch64" ;;
  Darwin:arm64)  PLATFORM="macos-arm64" ;;
  *) die "unsupported platform $OS/$ARCH — grab a tarball from https://github.com/$REPO/releases" ;;
esac
IS_MACOS=false
case "$OS" in Darwin) IS_MACOS=true ;; esac

# --- resolve the latest version ---------------------------------------------
API_URL="https://api.github.com/repos/$REPO/releases/latest"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if fetch "$API_URL" "$TMP/release.json"; then
  VERSION="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' "$TMP/release.json" | head -n 1)"
fi
[ -n "${VERSION:-}" ] || die "could not determine the latest release from $API_URL"

# --- download + unpack -------------------------------------------------------
say "installing komet $VERSION ($PLATFORM)…"
if $IS_MACOS; then
  # macOS ships Komet.app inside a -app tarball (auto-updater layout) and a dmg.
  TARBALL="komet-$VERSION-$PLATFORM-app.tar.gz"
else
  TARBALL="komet-$VERSION-$PLATFORM.tar.gz"
fi
URL="https://github.com/$REPO/releases/download/v$VERSION/$TARBALL"
fetch "$URL" "$TMP/$TARBALL" || die "download failed: $URL"
mkdir -p "$TMP/unpacked"
tar -xzf "$TMP/$TARBALL" -C "$TMP/unpacked"

if $IS_MACOS; then
  APP_SRC="$TMP/unpacked/Komet.app"
  [ -d "$APP_SRC" ] || die "tarball did not contain Komet.app"
  say "installing Komet.app into /Applications…"
  rm -rf "/Applications/Komet.app"
  cp -R "$APP_SRC" /Applications/
  say ""
  say "installed komet $VERSION → /Applications/Komet.app"
  say "next steps: launch Komet from Applications or Spotlight."
  exit 0
fi

# The tarball is wrapped in a versioned top-level directory
# (scripts/package-linux.sh tars the whole stage dir), so splice those
# contents up a level when the binary isn't at the extraction root. A flat
# tarball passes through untouched — and the same nesting is expected by
# crates/update (stage_headless), which unpacks with --strip-components=1.
if [ ! -f "$TMP/unpacked/komet" ]; then
  INNER="$(find "$TMP/unpacked" -mindepth 1 -maxdepth 1 -type d -name 'komet-*' | head -n 1)"
  if [ -n "$INNER" ]; then
    cp -R "$INNER/." "$TMP/unpacked/"
    rm -rf "$INNER"
  fi
fi

[ -f "$TMP/unpacked/komet" ] || die "tarball did not contain a komet binary"

# --- install into the managed layout ----------------------------------------
APP_ROOT="$PREFIX/app"
DEST="$APP_ROOT/$VERSION"
mkdir -p "$DEST"
cp "$TMP/unpacked/komet" "$DEST/komet"
chmod 755 "$DEST/komet"

# Atomic symlink flip: same layout the self-updater uses. we create the temp
# symlink → $DEST first, then rename it over `current` (the `-T` prevents
# plain `mv` from moving it INSIDE the target dir if `current` is itself a
# symlink-to-directory), so never a window with no `current` present.
ln -s "$DEST" "$APP_ROOT/.current-new.$$"
mv -fT "$APP_ROOT/.current-new.$$" "$APP_ROOT/current"

mkdir -p "$BIN_DIR"
ln -sf "$APP_ROOT/current/komet" "$BIN_DIR/komet"

# Desktop entry + icon when the tarball carries them.
[ -f "$TMP/unpacked/komet.desktop" ] && {
  mkdir -p "$HOME/.local/share/applications"
  cp "$TMP/unpacked/komet.desktop" "$HOME/.local/share/applications/komet.desktop"
}
[ -f "$TMP/unpacked/komet.png" ] && {
  mkdir -p "$HOME/.local/share/icons/hicolor/1024x1024/apps"
  cp "$TMP/unpacked/komet.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/komet.png"
}
[ -d "$TMP/unpacked/icons/hicolor" ] && {
  mkdir -p "$HOME/.local/share/icons"
  cp -r "$TMP/unpacked/icons/hicolor" "$HOME/.local/share/icons/"
}

# Refresh icon + desktop caches so the launcher icon appears immediately
# (without requiring a relogin / reboot). Failures are non-fatal — the icon
# will appear after the next cache rebuild or session restart.
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    say ""
    say "note: add $BIN_DIR to your PATH, e.g.:"
    say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc"
    ;;
esac

say ""
say "installed komet $VERSION → $DEST"
say "next steps:"
say "  komet              # launch the app"
say "  komet daemon install   # run the engine as a service"
say "  komet sync-init        # set up self-hosted sync"
