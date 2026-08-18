#!/usr/bin/env bash
# Windows packaging (runs in Git Bash on windows runners): build the release
# binary for x86_64-pc-windows-msvc and produce
#   target/package/komet-<version>-windows-x86_64.exe   (portable)
#   target/package/komet-<version>-windows-x86_64.msi   (installer)
#
# Usage: scripts/package-windows.sh
# Env:   WINDOWS_CERT_P12 — base64-encoded Authenticode .p12; when set (with
#        WINDOWS_CERT_PASSWORD) the exe and msi are signed with signtool.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
TARGET="${TARGET:-x86_64-pc-windows-msvc}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
EXE="$OUT_DIR/komet-$VERSION-windows-x86_64.exe"
MSI="$OUT_DIR/komet-$VERSION-windows-x86_64.msi"

cd "$ROOT"
rustup target add "$TARGET"
cargo build --release --target "$TARGET" -p komet

mkdir -p "$OUT_DIR"
rm -f "$EXE" "$MSI"
install -m 755 "$ROOT/target/$TARGET/release/komet.exe" "$EXE"

P12=""
if [[ -n "${WINDOWS_CERT_P12:-}" ]]; then
  P12="$OUT_DIR/windows-cert.p12"
  # base64 on stdin keeps the secret out of the process arguments; the file is
  # removed before the artifacts are uploaded.
  printf '%s' "$WINDOWS_CERT_P12" | base64 -d >"$P12"
  command -v signtool >/dev/null 2>&1 ||
    PATH="$(dirname "$(find '/c/Program Files (x86)/Windows Kits/10/bin' \
      -name signtool.exe -path '*x64*' | sort | tail -1)"):$PATH"
fi

# Sign the exe before the MSI is built so the embedded copy carries the
# signature too, then sign the MSI itself.
sign() {
  [[ -n "$P12" ]] || return 0
  signtool sign \
    -f "$P12" -p "${WINDOWS_CERT_PASSWORD:-}" \
    -fd sha256 -tr http://timestamp.digicert.com -td sha256 \
    "$1"
}
sign "$EXE"

# WiX v5 ships as a dotnet tool; the runner images have the .NET SDK.
if ! command -v wix >/dev/null 2>&1; then
  dotnet tool install --global wix >/dev/null
  PATH="$HOME/.dotnet/tools:$PATH"
fi

# wix is a native Windows tool: hand it Windows paths, not the MSYS ones.
wix build \
  -arch x64 \
  -define "Version=$VERSION" \
  -define "KometExe=$(cygpath -w "$EXE")" \
  -out "$(cygpath -w "$MSI")" \
  "$(cygpath -w "$ROOT/dist/windows/installer.wxs")"

sign "$MSI"
[[ -z "$P12" ]] || rm -f "$P12"

echo "packaged: $EXE"
echo "packaged: $MSI"
