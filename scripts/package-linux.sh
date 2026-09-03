#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/komet-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/komet-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/komet"
strip --strip-unneeded "$STAGE/komet" 2>/dev/null || true
install -m 644 "$ROOT/dist/komet.desktop" "$STAGE/komet.desktop"
install -m 644 "$ROOT/dist/komet.png" "$STAGE/komet.png"
cp -r "$ROOT/dist/icons" "$STAGE/icons"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Komet into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/komet" "$HOME/.local/bin/komet"
install -Dm644 "$HERE/komet.desktop" "$HOME/.local/share/applications/komet.desktop"
mkdir -p "$HOME/.local/share/icons"
if [[ -d "$HERE/icons/hicolor" ]]; then
  cp -r "$HERE/icons/hicolor" "$HOME/.local/share/icons/"
else
  install -Dm644 "$HERE/komet.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/komet.png"
fi
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" || true
echo "Installed. Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
