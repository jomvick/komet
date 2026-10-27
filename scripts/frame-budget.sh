#!/usr/bin/env bash
# Frame-budget baseline: run the headed app with `KOMET_WINDOW_STATS=1`,
# aggregate the "window frame budget" windows, print a table.
#
#   scripts/frame-budget.sh                 # 20s, seeded demo data if present
#   scripts/frame-budget.sh --seconds 40    # longer capture
#   scripts/frame-budget.sh --csv out.csv   # also write every window to CSV
#   scripts/frame-budget.sh --debug         # measure the dev binary instead
#
# Release by default, deliberately: `[profile.dev]` leaves the workspace
# crates at `opt-level = 0` (root Cargo.toml), so a debug build inflates
# exactly what is measured — the `render` cost and the missed-frame count —
# and makes the 16.7 ms budget meaningless. Use `--debug` only to check that
# the instrumentation fires, never to record a baseline.
#
# The numbers only mean something while something animates: hover the
# sidebar, scroll a transcript, or let a reply stream. An idle window shows
# long intervals because nothing was scheduled — those are excluded from the
# samples by `crates/ui/src/perf.rs`, but the capture still needs load. An
# unfocused window is excluded too (gpui throttles it to ~30 fps).
#
# The table splits the frame into `build` (Shell's own render body),
# `children` (Transcript/Composer/Changes renders) and `total`. The
# `renders/frame` line counts child renders: with view caching active, a
# sidebar-only animation must leave the models at 0.
#
# Needs a graphical session and a running demo daemon: start
# `scripts/dev-demo.sh --daemon-only` first (or a `komet headless` engine on
# `KOMET_IPC_PORT`, default 27921). Reuses /tmp/komet-demo-ui when
# `dev-demo.sh` has seeded it, so the transcript is realistic.
set -euo pipefail
cd "$(dirname "$0")/.."

SECONDS_TO_RUN=20
CSV=""
PROFILE=release
while [[ $# -gt 0 ]]; do
  case "$1" in
    --seconds) SECONDS_TO_RUN="$2"; shift 2 ;;
    --csv) CSV="$2"; shift 2 ;;
    --debug) PROFILE=debug; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

BIN=./target/$PROFILE/komet
if [[ ! -x "$BIN" ]]; then
  echo "▸ no $BIN yet — building $PROFILE (first run takes a while)…"
  if [[ "$PROFILE" == release ]]; then
    cargo build --release -p komet -q
  else
    cargo build -p komet -q
  fi
fi

DATA_DIR="${KOMET_DATA_DIR:-/tmp/komet-demo-ui}"
IPC="${KOMET_IPC_PORT:-27921}"
# Authenticated probe: an open port proves nothing — the RPC handshake
# presents the token published for this port, and `LocalDevice` only answers
# from a Komet engine, so another process on the port cannot pass this check.
probe() { cargo run -q -p komet-rpc --example rpc_probe -- "ws://127.0.0.1:$IPC" "$@"; }
if ! DEVICE_JSON=$(probe LocalDevice '{}' 2>/dev/null); then
  echo "▸ no authenticated engine on 127.0.0.1:$IPC — start scripts/dev-demo.sh first" >&2
  exit 1
fi
if ! python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("deviceId") else 1)' \
  <<<"$DEVICE_JSON"; then
  echo "▸ 127.0.0.1:$IPC answered without a Komet device identity — refusing to capture" >&2
  exit 1
fi
LOG=$(mktemp -t komet-frame-budget.XXXXXX.log)
echo "▸ capturing ${SECONDS_TO_RUN}s into $LOG (data: $DATA_DIR, daemon: :$IPC)"

set +e
env KOMET_DATA_DIR="$DATA_DIR" KOMET_IPC_PORT="$IPC" KOMET_HARNESS=mock \
  KOMET_WINDOW_STATS=1 RUST_LOG=warn \
  timeout "${SECONDS_TO_RUN}s" "$BIN" >"$LOG" 2>&1
STATUS=$?
set -e
# 124 is `timeout` ending the capture; anything else is the app failing.
if [[ $STATUS -ne 0 && $STATUS -ne 124 ]]; then
  echo "▸ the app exited with $STATUS — last lines:" >&2
  tail -20 "$LOG" >&2
  exit "$STATUS"
fi

python3 - "$LOG" "${CSV:-}" <<'PY'
import re, statistics, sys

log_path, csv_path = sys.argv[1], sys.argv[2]
ansi_re = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
window_re = re.compile(r'window frame budget\b')
kv_re = re.compile(r"(\w+)=(\d+)")

windows = []
for line in open(log_path, encoding="utf-8", errors="replace"):
    line = ansi_re.sub("", line)
    if not window_re.search(line):
        continue
    fields = {k: int(v) for k, v in kv_re.findall(line)}
    if "frames" in fields and "interval_p50_us" in fields:
        windows.append(fields)

active_windows = [w for w in windows if w["frames"] > 0]
total_frames = sum(w["frames"] for w in active_windows)
if not active_windows:
    print("▸ no active frames captured — hover, scroll, or stream during the capture")
    sys.exit(1)

def med(key):
    return statistics.median(w[key] for w in active_windows)

def ratio(key):
    return sum(w[key] for w in active_windows) / total_frames

def ms(key):
    return f"{med(key) / 1000:.1f}"

def peak_ms(key):
    return f"{max(w[key] for w in active_windows) / 1000:.1f}"

print()
print(f"fenêtres actives: {len(active_windows)}   frames actives: {total_frames}")
print(f"frames manquées: {ratio('missed') * 100:.1f} % (> 16.7 ms)")
print(f"{'':<10}{'p50':>8}{'p95':>8}{'p99':>8}{'max':>8}   (ms)")
print(f"{'interval':<10}{ms('interval_p50_us'):>8}{ms('interval_p95_us'):>8}"
      f"{ms('interval_p99_us'):>8}{peak_ms('interval_max_us'):>8}")
print(f"{'build':<10}{ms('build_p50_us'):>8}{ms('build_p95_us'):>8}"
      f"{ms('build_p99_us'):>8}{peak_ms('build_max_us'):>8}")
print(f"{'children':<10}{ms('children_p50_us'):>8}{ms('children_p95_us'):>8}"
      f"{ms('children_p99_us'):>8}{peak_ms('children_max_us'):>8}")
print(f"{'total':<10}{ms('total_p50_us'):>8}{ms('total_p95_us'):>8}"
      f"{ms('total_p99_us'):>8}{peak_ms('total_max_us'):>8}")
print(f"renders/frame: transcript={ratio('transcript_renders'):.2f}"
      f"  composer={ratio('composer_renders'):.2f}"
      f"  changes={ratio('changes_renders'):.2f}")

if csv_path:
    keys = ["frames", "missed", "interval_p50_us", "interval_p95_us",
            "interval_p99_us", "interval_max_us", "build_p50_us",
            "build_p95_us", "build_p99_us", "build_max_us",
            "children_p50_us", "children_p95_us", "children_p99_us",
            "children_max_us", "total_p50_us", "total_p95_us",
            "total_p99_us", "total_max_us", "transcript_renders",
            "composer_renders", "changes_renders"]
    with open(csv_path, "w", encoding="utf-8") as out:
        out.write(",".join(keys) + "\n")
        for w in active_windows:
            out.write(",".join(str(w[k]) for k in keys) + "\n")
    print(f"\nCSV: {csv_path}")
PY
