#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
APPDIR="$OUT_DIR/Komet.AppDir"
APPIMAGE="$OUT_DIR/komet-${VERSION}-linux-${ARCH}.AppImage"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet"
fi

rm -rf "$APPDIR" "$APPIMAGE"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons"

install -m 755 "$BIN" "$APPDIR/usr/bin/komet"
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/usr/share/applications/komet.desktop"
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/komet.desktop"
cp -r "$ROOT/dist/icons/hicolor" "$APPDIR/usr/share/icons/"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/komet.png"
# top-level icon required by AppImage spec
cp "$APPDIR/komet.png" "$APPDIR/.DirIcon"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
export APPDIR="$HERE"
export PATH="$HERE/usr/bin:$PATH"
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib64:${LD_LIBRARY_PATH:-}"
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"

# First-launch desktop integration (AppImage spec: the payload MAY install
# a .desktop file). There is no Type 2 `--appimage-integrate` flag — do it
# here. Skip when the user opted out or another integrator is in charge.
# Failures are swallowed so the app always launches.
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
if [ -n "${APPIMAGE:-}" ] \
   && [ -z "${DESKTOPINTEGRATION:-}" ] \
   && [ ! -e "$DATA_HOME/appimagekit/no_desktopintegration" ] \
   && [ ! -e "$HOME/.local/share/appimagekit/no_desktopintegration" ] \
   && [ ! -e /usr/share/appimagekit/no_desktopintegration ] \
   && [ ! -e /etc/appimagekit/no_desktopintegration ]; then
  HASH="$(printf '%s' "$APPIMAGE" | md5sum | cut -c1-8)"
  SENTINEL="${XDG_CONFIG_HOME:-$HOME/.config}/komet/appimage-integrated-$HASH"
  if [ ! -f "$SENTINEL" ]; then
    APPS="$DATA_HOME/applications"
    ICONS="$DATA_HOME/icons"
    mkdir -p "$APPS" "$ICONS" "$(dirname "$SENTINEL")" || true
    if [ -d "$HERE/usr/share/icons/hicolor" ]; then
      cp -r "$HERE/usr/share/icons/hicolor" "$ICONS/" || true
    elif [ -f "$HERE/komet.png" ]; then
      mkdir -p "$ICONS/hicolor/256x256/apps" || true
      cp "$HERE/komet.png" "$ICONS/hicolor/256x256/apps/komet.png" || true
    fi
    DESKTOP_SRC="$HERE/usr/share/applications/komet.desktop"
    [ -f "$DESKTOP_SRC" ] || DESKTOP_SRC="$HERE/komet.desktop"
    if [ -f "$DESKTOP_SRC" ]; then
      ESCAPED="$(printf '%s' "$APPIMAGE" | sed 's/\\/\\\\/g; s/"/\\"/g')"
      {
        grep -v -E '^(Exec|TryExec)=' "$DESKTOP_SRC" || true
        printf 'Exec="%s" %%U\n' "$ESCAPED"
      } >"$APPS/komet.desktop" || true
    fi
    command -v update-desktop-database >/dev/null 2>&1 \
      && update-desktop-database "$APPS" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
      && gtk-update-icon-cache -f -t "$ICONS/hicolor" 2>/dev/null || true
    touch "$SENTINEL" || true
  fi
fi

exec "$HERE/usr/bin/komet" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# Build tools are pinned to fixed releases and checked against their sha256,
# because both end up inside the published AppImage. Update version and
# checksum together (the checksums are listed on each GitHub release).
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
RUNTIME_VERSION="20251108"
case "$ARCH" in
  x86_64) RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d" ;;
  aarch64) RUNTIME_SHA256="00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444" ;;
  *) echo "no pinned AppImage runtime for $ARCH" >&2; exit 1 ;;
esac

# download_verified <url> <dest> <sha256>: fetch to a temporary file and keep
# it only when the checksum matches.
download_verified() {
  local url="$1" dest="$2" expected="$3"
  curl -fsSL -o "$dest.partial" "$url"
  if ! echo "$expected  $dest.partial" | sha256sum -c --quiet -; then
    rm -f "$dest.partial"
    echo "checksum mismatch for $url" >&2
    exit 1
  fi
  mv "$dest.partial" "$dest"
}

# ensure_verified <url> <dest> <sha256>: reuse <dest> only when it matches the
# checksum; otherwise (missing, modified or partial) download it again.
ensure_verified() {
  local url="$1" dest="$2" expected="$3"
  if [[ -f "$dest" ]] && echo "$expected  $dest" | sha256sum -c --quiet - 2>/dev/null; then
    return 0
  fi
  echo "Downloading $url..."
  rm -f "$dest"
  download_verified "$url" "$dest" "$expected"
}

TOOL="$OUT_DIR/appimagetool-$APPIMAGETOOL_VERSION.AppImage"
ensure_verified \
  "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage" \
  "$TOOL" "$APPIMAGETOOL_SHA256"
chmod +x "$TOOL"

RUNTIME="$OUT_DIR/runtime-$RUNTIME_VERSION-$ARCH"
ensure_verified \
  "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
  "$RUNTIME" "$RUNTIME_SHA256"

EXTRA_ARGS=()
if [[ -f "$RUNTIME" && -s "$RUNTIME" ]]; then
  EXTRA_ARGS+=(--runtime-file "$RUNTIME")
fi

# Build AppImage (extract and run if FUSE unavailable)
(
  cd "$OUT_DIR"
  rm -rf squashfs-root
  ARCH="$ARCH" "$TOOL" --appimage-extract >/dev/null 2>&1 || true
  if [[ -d "squashfs-root" ]]; then
    ARCH="$ARCH" ./squashfs-root/AppRun "${EXTRA_ARGS[@]}" "$APPDIR" "$APPIMAGE"
    rm -rf squashfs-root
  else
    ARCH="$ARCH" "$TOOL" "${EXTRA_ARGS[@]}" "$APPDIR" "$APPIMAGE"
  fi
)

# Clean up temporary squashfs-root in workspace if any
rm -rf "$ROOT/squashfs-root"

chmod +x "$APPIMAGE"
echo "AppImage: $APPIMAGE"
ls -lh "$APPIMAGE"
