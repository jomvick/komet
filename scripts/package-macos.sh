#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/komet-<version>-macos-<arch>.dmg
# containing Komet.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Komet.app"
DMG="$OUT_DIR/komet-$VERSION-macos-$ARCH.dmg"

cd "$ROOT"
cargo build --release -p komet

rm -rf "$APP" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/komet" "$APP/Contents/MacOS/komet"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Icon: iconset from the placeholder png (replace dist/komet.png to rebrand).
ICONSET="$OUT_DIR/komet.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/dist/komet.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/komet.icns"
rm -rf "$ICONSET"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --deep --force --options runtime --sign "$CODESIGN_IDENTITY" "$APP"
else
  # Ad-hoc signature so the app launches on Apple silicon (Gatekeeper still
  # requires right-click → Open on first launch without notarization).
  codesign --deep --force --sign - "$APP"
fi

hdiutil create -volname Komet -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "packaged: $DMG"
