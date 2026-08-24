#!/usr/bin/env bash
# Windows packaging: build the release binary for Windows and produce
#   target/package/komet-<version>-windows-x86_64.zip
#   target/package/komet-<version>-windows-x86_64.exe
#
# Usage: scripts/package-windows.sh
# Env:   PROFILE=debug for unoptimized build; default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$USERPROFILE/.cargo/bin:$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="x86_64"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/komet-$VERSION-windows-$ARCH"
ZIP="$STAGE.zip"
PORTABLE_EXE="$OUT_DIR/komet-$VERSION-windows-$ARCH.exe"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet.exe"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet.exe"
fi

rm -rf "$STAGE" "$ZIP" "$PORTABLE_EXE"
mkdir -p "$STAGE" "$OUT_DIR"

cp "$BIN" "$STAGE/komet.exe"
cp "$BIN" "$PORTABLE_EXE"
[[ -f "$ROOT/README.md" ]] && cp "$ROOT/README.md" "$STAGE/"
[[ -f "$ROOT/LICENSE" ]] && cp "$ROOT/LICENSE" "$STAGE/"

# Create zip using 7z, powershell, or zip
if command -v 7z >/dev/null 2>&1; then
  7z a "$ZIP" "$STAGE/*" >/dev/null
elif command -v powershell >/dev/null 2>&1; then
  powershell -NoProfile -Command "Compress-Archive -Path '$STAGE/*' -DestinationPath '$ZIP' -Force"
elif command -v zip >/dev/null 2>&1; then
  (cd "$OUT_DIR" && zip -r "$(basename "$ZIP")" "$(basename "$STAGE")")
fi

rm -rf "$STAGE"
echo "packaged: $ZIP"
echo "portable: $PORTABLE_EXE"
