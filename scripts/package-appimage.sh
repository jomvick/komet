#!/usr/bin/env bash
# Linux AppImage packaging: build the release binary and produce
#   target/package/komet-<version>-linux-<arch>.AppImage
# a self-contained portable app (binary + .desktop entry + icon + AppRun).
#
# Usage: scripts/package-appimage.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.
#        APPIMAGETOOL=/path/to/appimagetool to reuse a local copy instead of
#        downloading the release runtime from GitHub.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
APPDIR="$OUT_DIR/komet-$VERSION-linux-$ARCH.AppDir"
APPIMAGE="$OUT_DIR/komet-$VERSION-linux-$ARCH.AppImage"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet"
fi

rm -rf "$APPDIR" "$APPIMAGE"
mkdir -p "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/1024x1024/apps"

install -m 755 "$BIN" "$APPDIR/usr/bin/komet"
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/usr/share/applications/komet.desktop"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/usr/share/icons/hicolor/1024x1024/apps/komet.png"
# AppImage spec: the .desktop entry and the icon must also sit at the AppDir root.
install -m 644 "$ROOT/dist/komet.desktop" "$APPDIR/komet.desktop"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/komet.png"
install -m 644 "$ROOT/dist/komet.png" "$APPDIR/.DirIcon"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
export PATH="$HERE/usr/bin:${PATH:-}"
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
exec "$HERE/usr/bin/komet" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

TOOL="${APPIMAGETOOL:-}"
if [[ -z "$TOOL" ]]; then
  if command -v appimagetool >/dev/null 2>&1; then
    TOOL="$(command -v appimagetool)"
  else
    TOOL="$OUT_DIR/appimagetool-$ARCH.AppImage"
    if [[ ! -x "$TOOL" ]]; then
      echo "downloading appimagetool for $ARCH"
      curl -fsSL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
      chmod 755 "$TOOL"
    fi
  fi
fi

# `--appimage-extract-and-run` avoids requiring FUSE inside containers/CI.
ARCH="$ARCH" "$TOOL" --appimage-extract-and-run --no-appstream "$APPDIR" "$APPIMAGE"

rm -rf "$APPDIR"
echo "packaged: $APPIMAGE"
