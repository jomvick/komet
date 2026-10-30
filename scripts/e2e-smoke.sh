#!/usr/bin/env bash
# Two-device e2e smoke: the local self-hosted sync server (komet-sync-server, no
# edge/ worker), two headless engines, and the komet-rpc e2e_driver example
# proving the doc-queued cross-device command path:
#
#   B queues a Run into the chat doc -> nudge -> A (host) executes via the mock
#   harness -> transcript + session status sync A -> server -> B.
#
# Both engines run as the SAME user (alice@org1) on different devices — komet's
# one-user-many-devices model; chat/device rooms are claim-on-first-join per user.
# The server authenticates every socket with one static bearer (KOMET_SYNC_TOKEN).
#
# Required binaries: `komet`, `komet-rpc`'s `e2e_driver` example and
# `komet-sync-server`. They are built here unless prebuilt ones are supplied
# with KOMET_E2E_KOMET_BIN, KOMET_E2E_DRIVER_BIN, KOMET_E2E_SYNC_BIN (each
# override skips its build — use this to verify without a rebuild).
#
# Usage: scripts/e2e-smoke.sh
# Env:   KOMET_E2E_SYNC_PORT (default 27640), KOMET_E2E_KEEP_LOGS=1 to keep logs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
# Cargo may be configured with a target directory other than $ROOT/target.
TARGET_DIR="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps --quiet \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SYNC_PORT="${KOMET_E2E_SYNC_PORT:-27640}"
SYNC_URL="http://127.0.0.1:${SYNC_PORT}"
SYNC_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
TOKEN="alice@org1"
ORG="org1"
A_PORT=27801
B_PORT=27802
TMP_DIR="$(mktemp -d /tmp/komet-e2e.XXXXXX)"
LOG_DIR="$TMP_DIR/logs"
SYNC_DIR="$TMP_DIR/sync"
A_DIR="$TMP_DIR/engine-a"
B_DIR="$TMP_DIR/engine-b"
mkdir -p "$LOG_DIR" "$SYNC_DIR" "$A_DIR" "$B_DIR"

SYNC_PID=""
A_PID=""
B_PID=""
STATUS=1

cleanup() {
  for pid in "$SYNC_PID" "$A_PID" "$B_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  sleep 1
  for pid in "$SYNC_PID" "$A_PID" "$B_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  rm -rf "$A_DIR" "$B_DIR" "$SYNC_DIR"
  if [[ "$STATUS" -ne 0 ]]; then
    echo "--- engine A log (tail) ---"; tail -n 40 "$LOG_DIR/engine-a.log" 2>/dev/null || true
    echo "--- engine B log (tail) ---"; tail -n 40 "$LOG_DIR/engine-b.log" 2>/dev/null || true
    echo "--- sync server log (tail) ---"; tail -n 40 "$LOG_DIR/sync-server.log" 2>/dev/null || true
  fi
  if [[ "${KOMET_E2E_KEEP_LOGS:-0}" != "1" ]]; then
    rm -rf "$TMP_DIR"
  else
    echo "logs kept in $LOG_DIR"
  fi
}
trap cleanup EXIT

wait_for() { # wait_for <description> <timeout_s> <command...>
  local what="$1" timeout="$2"; shift 2
  local waited=0
  until "$@" >/dev/null 2>&1; do
    sleep 1
    waited=$((waited + 1))
    if [[ "$waited" -ge "$timeout" ]]; then
      echo "FAIL: timed out waiting for $what" >&2
      exit 1
    fi
  done
}

# ── 1. Build the binaries (workspace target is warm in CI/dev) ─────────────────
echo "build: komet + komet-sync-server + e2e_driver (overrides skip their build)"
# Two invocations: a single one with `--example` builds ONLY the example
# (the target filter applies across every -p), silently skipping the komet
# bin — the smoke then dies on "No such file or directory".
if [[ -n "${KOMET_E2E_KOMET_BIN:-}" ]]; then
  KOMET="$KOMET_E2E_KOMET_BIN"
else
  (cd "$ROOT" && cargo build -q -p komet)
  KOMET="$TARGET_DIR/debug/komet"
fi
if [[ -n "${KOMET_E2E_SYNC_BIN:-}" ]]; then
  SYNC="$KOMET_E2E_SYNC_BIN"
else
  (cd "$ROOT" && cargo build -q -p komet-sync-server)
  SYNC="$TARGET_DIR/debug/komet-sync-server"
fi
if [[ -n "${KOMET_E2E_DRIVER_BIN:-}" ]]; then
  DRIVER="$KOMET_E2E_DRIVER_BIN"
else
  (cd "$ROOT" && cargo build -q -p komet-rpc --example e2e_driver)
  DRIVER="$TARGET_DIR/debug/examples/e2e_driver"
fi

# ── 2. Local self-hosted sync server (one static bearer) ───────────────────────
# Never adopt a foreign listener on this port: the engines would sign in against
# a server with another token and every room join would 401.
if (exec 3<>"/dev/tcp/127.0.0.1/$SYNC_PORT") 2>/dev/null; then
  echo "FAIL: sync port $SYNC_PORT is already in use — refusing to reuse a foreign server" >&2
  echo "      (pick another one with KOMET_E2E_SYNC_PORT)" >&2
  exit 1
fi
echo "sync: starting komet-sync-server on :$SYNC_PORT"
KOMET_SYNC_TOKEN="$SYNC_TOKEN" KOMET_SYNC_PORT="$SYNC_PORT" KOMET_DATA_DIR="$SYNC_DIR" \
  RUST_LOG=info "$SYNC" >"$LOG_DIR/sync-server.log" 2>&1 &
SYNC_PID=$!
wait_for "sync server /health :$SYNC_PORT" 30 curl -sf -m 3 "$SYNC_URL/health"
curl -sf -m 3 -H "Authorization: Bearer $SYNC_TOKEN" "$SYNC_URL/env" >/dev/null

# ── 3. Two headless engines, one user, two devices ─────────────────────────────
start_engine() { # start_engine <data_dir> <ipc_port> <name> <log>
  KOMET_DATA_DIR="$1" KOMET_IPC_PORT="$2" KOMET_DEVICE_NAME="$3" \
    KOMET_EDGE_URL="$SYNC_URL" KOMET_SYNC_TOKEN="$SYNC_TOKEN" \
    KOMET_EDGE_TOKEN="$TOKEN" KOMET_ORG_ID="$ORG" \
    KOMET_HARNESS=mock RUST_LOG=info \
    "$KOMET" headless >"$4" 2>&1 &
}

start_engine "$A_DIR" "$A_PORT" "e2e-device-a" "$LOG_DIR/engine-a.log"; A_PID=$!
start_engine "$B_DIR" "$B_PORT" "e2e-device-b" "$LOG_DIR/engine-b.log"; B_PID=$!

wait_for "engine A ipc :$A_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$A_PORT"
wait_for "engine B ipc :$B_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$B_PORT"
echo "engines: A pid=$A_PID ipc=:$A_PORT  B pid=$B_PID ipc=:$B_PORT"

# ── 4. Drive the cross-device flow through both IPCs ───────────────────────────
"$DRIVER" "$A_PORT" "$B_PORT"
STATUS=0
exit "$STATUS"
