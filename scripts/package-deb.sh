#!/usr/bin/env bash
# Debian/Ubuntu packaging: build release binary and produce
#   target/package/komet_<version>_<arch>.deb
#
# Usage: scripts/package-deb.sh
# Env:   PROFILE=debug for fast unoptimized package; default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"

UNAME_ARCH="$(uname -m)"
case "$UNAME_ARCH" in
  x86_64)  DEB_ARCH="amd64" ;;
  aarch64) DEB_ARCH="arm64" ;;
  *)       DEB_ARCH="$UNAME_ARCH" ;;
esac

OUT_DIR="$ROOT/target/package"
DEB_NAME="komet_${VERSION}_${DEB_ARCH}.deb"
DEB_FILE="$OUT_DIR/$DEB_NAME"
STAGE_DIR="$OUT_DIR/deb-stage"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet"
fi

rm -rf "$STAGE_DIR" "$DEB_FILE"
mkdir -p "$STAGE_DIR/usr/bin" \
         "$STAGE_DIR/usr/share/applications" \
         "$STAGE_DIR/usr/share/icons/hicolor" \
         "$STAGE_DIR/usr/share/pixmaps" \
         "$STAGE_DIR/DEBIAN"

# Install files
install -m 755 "$BIN" "$STAGE_DIR/usr/bin/komet"
strip --strip-unneeded "$STAGE_DIR/usr/bin/komet" 2>/dev/null || true
install -m 644 "$ROOT/dist/komet.desktop" "$STAGE_DIR/usr/share/applications/komet.desktop"
cp -r "$ROOT/dist/icons/hicolor"/* "$STAGE_DIR/usr/share/icons/hicolor/"
install -m 644 "$ROOT/dist/komet.png" "$STAGE_DIR/usr/share/pixmaps/komet.png"

# Calculate installed size in KB
INSTALLED_SIZE="$(du -sk "$STAGE_DIR/usr" | awk '{print $1}')"

# Control file
cat >"$STAGE_DIR/DEBIAN/control" <<EOF
Package: komet
Version: ${VERSION}
Architecture: ${DEB_ARCH}
Maintainer: Komet Team <support@komet.sh>
Installed-Size: ${INSTALLED_SIZE}
Section: devel
Priority: optional
Homepage: https://komet.sh
Description: Multi-device controller for coding agents
 Komet is a unified coding agent controller supporting
 autonomous agent loops, terminal sandboxing, and multi-device sync.
EOF

# Post-install & Post-remove hooks
cat >"$STAGE_DIR/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
EOF
chmod 755 "$STAGE_DIR/DEBIAN/postinst"

cat >"$STAGE_DIR/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
EOF
chmod 755 "$STAGE_DIR/DEBIAN/postrm"

# Build DEB
if command -v dpkg-deb >/dev/null 2>&1; then
  dpkg-deb --build --root-owner-group "$STAGE_DIR" "$DEB_FILE"
else
  # Portable assembly via standard ar + tar (works anywhere without dpkg)
  echo "2.0" > "$STAGE_DIR/debian-binary"
  (
    cd "$STAGE_DIR/DEBIAN"
    tar --owner=0 --group=0 -czf "$STAGE_DIR/control.tar.gz" .
  )
  (
    cd "$STAGE_DIR"
    tar --owner=0 --group=0 -czf "$STAGE_DIR/data.tar.gz" ./usr
  )
  (
    cd "$STAGE_DIR"
    ar -rc "$DEB_FILE" debian-binary control.tar.gz data.tar.gz
  )
fi

rm -rf "$STAGE_DIR"
echo "packaged: $DEB_FILE"
ls -lh "$DEB_FILE"
