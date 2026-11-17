#!/usr/bin/env bash
# First-run onboarding demo: a throwaway data dir so the tour always shows,
# against the real engine (real agent CLIs detected, no `KOMET_HARNESS=mock`).
#
#   scripts/onboarding-demo.sh             # fresh tour from Welcome
#   scripts/onboarding-demo.sh agents      # jump to a step
#                                          # welcome|agents|appearance|permissions|done
#   scripts/onboarding-demo.sh --keep      # reuse the last data dir (flag stays set)
#
# The data dir is printed; delete it (or re-run) to see the tour again.
set -euo pipefail
cd "$(dirname "$0")/.."

STEP=""
KEEP=""
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    welcome|agents|appearance|permissions|done|security) STEP="$arg" ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

echo "▸ building (first run takes a few minutes)…"
cargo build -p komet -q

DATA_DIR=/tmp/komet-onboarding-demo
if [[ -z "$KEEP" ]]; then
  rm -rf "$DATA_DIR"
fi
mkdir -p "$DATA_DIR"

echo "▸ onboarding demo — data: $DATA_DIR${STEP:+, step: $STEP}"
echo "  Ctrl-C to stop the UI (the engine child outlives it, like the app)"
KOMET_DATA_DIR="$DATA_DIR" RUST_LOG=warn ${STEP:+KOMET_ONBOARDING=$STEP} \
  ./target/debug/komet
