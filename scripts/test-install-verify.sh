#!/usr/bin/env sh
# Tests the release verification in install.sh with a throwaway key.
# Run with a POSIX shell and OpenSSL 3:  sh scripts/test-install-verify.sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
failures=0
check() { # check <description> <command...>
  desc="$1"; shift
  if "$@" >/dev/null 2>&1; then echo "ok: $desc"; else echo "FAILED: $desc"; failures=$((failures + 1)); fi
}
refuses() { # refuses <description> <command...>
  desc="$1"; shift
  if "$@" >/dev/null 2>&1; then echo "FAILED: $desc"; failures=$((failures + 1)); else echo "ok: $desc"; fi
}

KOMET_INSTALL_SOURCE_ONLY=1
. "$ROOT/install.sh"

cd "$WORK"
printf 'komet binary\n' > komet-1.2.3-linux-x86_64.tar.gz
SHA="$(sha256_of komet-1.2.3-linux-x86_64.tar.gz)"
cat > manifest.json <<JSON
{
  "version": "1.2.3",
  "files": {
    "komet-1.2.3-linux-x86_64.tar.gz": {
      "sha256": "$SHA"
    },
    "komet-1.2.3-linux-x86_64.tar.gz.bak": {
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }
  }
}
JSON
OPENSSL="$(find_openssl)"
"$OPENSSL" genpkey -algorithm ed25519 -out key.pem
"$OPENSSL" pkeyutl -sign -rawin -inkey key.pem -in manifest.json -out manifest.json.sig
PUB="$("$OPENSSL" pkey -in key.pem -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')"
"$OPENSSL" genpkey -algorithm ed25519 -out other.pem
OTHER="$("$OPENSSL" pkey -in other.pem -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')"

check "valid signature is accepted" verify_manifest_signature manifest.json manifest.json.sig "$PUB"
refuses "signature from another key is refused" verify_manifest_signature manifest.json manifest.json.sig "$OTHER"
refuses "empty public key is refused" verify_manifest_signature manifest.json manifest.json.sig ""
cp manifest.json tampered.json && printf ' ' >> tampered.json
refuses "tampered manifest is refused" verify_manifest_signature tampered.json manifest.json.sig "$PUB"

check "checksum is read for the exact file name" test "$(manifest_sha256 manifest.json komet-1.2.3-linux-x86_64.tar.gz 1.2.3)" = "$SHA"
refuses "a file missing from the manifest is refused" manifest_sha256 manifest.json komet-9.9.9-linux-x86_64.tar.gz 1.2.3
refuses "a manifest for another version is refused" manifest_sha256 manifest.json komet-1.2.3-linux-x86_64.tar.gz 1.2.4

check "matching download passes" verify_download komet-1.2.3-linux-x86_64.tar.gz "$SHA"
printf 'evil\n' > evil.tar.gz
refuses "modified download is refused" verify_download evil.tar.gz "$SHA"

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all install.sh verification checks passed"
