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
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/1024x1024/apps"

install -m 755 "$BIN" "$APPDIR/usr/bin/komet"
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/usr/share/applications/komet.desktop"
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/komet.desktop"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/usr/share/icons/hicolor/1024x1024/apps/komet.png"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/komet.png"
# top-level icon required by AppImage spec
cp "$APPDIR/komet.png" "$APPDIR/.DirIcon"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/komet" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# Download appimagetool if needed
TOOL="$OUT_DIR/appimagetool.AppImage"
if [[ ! -x "$TOOL" ]]; then
  echo "Downloading appimagetool..."
  curl -fsSL -o "$TOOL" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
  chmod +x "$TOOL"
fi

RUNTIME="$OUT_DIR/runtime-$ARCH"
if [[ ! -f "$RUNTIME" ]]; then
  echo "Downloading runtime-$ARCH..."
  curl -fsSL -o "$RUNTIME" "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-$ARCH" 2>/dev/null || true
fi

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
