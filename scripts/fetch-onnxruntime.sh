#!/usr/bin/env bash
# Fetch the official ONNX Runtime binaries and install the runtime library
# into <output-dir> under the name komet looks for next to the executable
# (libonnxruntime.so / libonnxruntime.dylib / onnxruntime.dll — see
# crates/memory/src/embeddings.rs).
#
# Why official binaries: ort's own prebuilt packages (cdn.pyke.io) are built
# against glibc >= 2.38 and macOS >= 13.4, which breaks the ubuntu-22.04 and
# macOS-11 release targets. The official Microsoft builds only need
# GLIBC_2.28 / an older macOS SDK.
#
# Usage: scripts/fetch-onnxruntime.sh <output-dir>
# Env:   ORT_VERSION  ONNX Runtime release to fetch (default 1.32.0).
#        ORT_DYLIB_NAME  override the installed file name (testing only).

set -euo pipefail

OUT_DIR="${1:?usage: fetch-onnxruntime.sh <output-dir>}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${ORT_VERSION:-1.32.0}"
CACHE="$ROOT/target/onnxruntime/$VERSION"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64)  ASSET="linux-x64" ;;
      aarch64) ASSET="linux-aarch64" ;;
      *) echo "no ONNX Runtime asset for linux-$arch" >&2; exit 1 ;;
    esac
    EXT="tgz"
    LIB_GLOB="lib/libonnxruntime.so"
    DEFAULT_NAME="libonnxruntime.so"
    ;;
  Darwin)
    case "$arch" in
      arm64)   ASSET="osx-arm64" ;;
      x86_64)  ASSET="osx-x64" ;;
      *) echo "no ONNX Runtime asset for macos-$arch" >&2; exit 1 ;;
    esac
    EXT="tgz"
    LIB_GLOB="lib/libonnxruntime.dylib"
    DEFAULT_NAME="libonnxruntime.dylib" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    ASSET="win-x64"
    EXT="zip"
    LIB_GLOB="lib/onnxruntime.dll"
    DEFAULT_NAME="onnxruntime.dll" ;;
  *)
    echo "unsupported OS for ONNX Runtime fetch: $os" >&2
    exit 1
    ;;
esac

URL="https://github.com/microsoft/onnxruntime/releases/download/v$VERSION/onnxruntime-$ASSET-$VERSION.$EXT"
WORK="$CACHE/$ASSET"
DEST_NAME="${ORT_DYLIB_NAME:-$DEFAULT_NAME}"

mkdir -p "$OUT_DIR" "$WORK"
if [[ ! -f "$WORK/lib/$DEST_NAME" ]]; then
  echo "Downloading $URL"
  TMP="$WORK/download.$EXT"
  curl -fsSL -o "$TMP" "$URL"
  rm -rf "$WORK/extract"
  mkdir -p "$WORK/extract"
  case "$EXT" in
    tgz) tar -xzf "$TMP" -C "$WORK/extract" ;;
    zip)
      if command -v unzip >/dev/null 2>&1; then
        unzip -q -o "$TMP" -d "$WORK/extract"
      elif command -v python >/dev/null 2>&1; then
        python -m zipfile -e "$TMP" "$WORK/extract"
      elif command -v python3 >/dev/null 2>&1; then
        python3 -m zipfile -e "$TMP" "$WORK/extract"
      else
        echo "need unzip or python to extract $URL" >&2
        exit 1
      fi
      ;;
  esac
  rm -f "$TMP"
  # Move the runtime lib next to the extracted tree root marker.
  PKG_DIR="$(find "$WORK/extract" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  [[ -n "$PKG_DIR" ]] || { echo "unexpected archive layout for $URL" >&2; exit 1; }
  mkdir -p "$WORK/lib"
  # cp -L follows the .so/.dylib symlink chain to the real library file.
  cp -L "$PKG_DIR/$LIB_GLOB" "$WORK/lib/$DEST_NAME"
fi

install -m 755 "$WORK/lib/$DEST_NAME" "$OUT_DIR/$DEST_NAME"
echo "onnxruntime: $OUT_DIR/$DEST_NAME (ORT $VERSION, $ASSET)"
