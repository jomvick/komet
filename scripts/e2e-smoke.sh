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
# Scenarios (first arg; default `commands`; `all` runs every scenario):
#   commands    B -> A command, A -> B transcript + session status (two-way).
#   restart     server killed between two runs: no loss, no duplicate, the
#               cursor resumes.
#   wake        restart, plus the suspend/resume hook: the script injects the
#               wake event (KOMET_TEST_WAKE_FILE) and greps the engines' logs
#               for the wake-triggered redial. SIGSTOP is NOT used: it freezes
#               monotonic vs wall clock in ways that do not model real suspend.
#   cut         engines dial through a TCP proxy; the proxy is killed and
#               restored (client-side cut) — resume must be under BUDGET_S.
#   checkpoint  1 MB checkpoint upload + cut mid-download, resumed by HTTP
#               Range and byte-compared to the original.
#
# Required binaries: `komet`, `komet-rpc`'s `e2e_driver` example and
# `komet-sync-server`. They are built here unless prebuilt ones are supplied
# with KOMET_E2E_KOMET_BIN, KOMET_E2E_DRIVER_BIN, KOMET_E2E_SYNC_BIN (each
# override skips its build — use this to verify without a rebuild).
#
# Usage: scripts/e2e-smoke.sh [commands|restart|wake|cut|checkpoint|all]
# Env:   KOMET_E2E_SYNC_PORT (default 27640), KOMET_E2E_KEEP_LOGS=1 to keep logs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
# Cargo may be configured with a target directory other than $ROOT/target.
TARGET_DIR="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps --quiet \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SCENARIO="${1:-commands}"
case "$SCENARIO" in commands|restart|wake|cut|checkpoint|all) ;; *)
  echo "FAIL: unknown scenario '$SCENARIO' (commands|restart|wake|cut|checkpoint|all)" >&2
  exit 1 ;;
esac
SYNC_PORT="${KOMET_E2E_SYNC_PORT:-27640}"
SYNC_URL="http://127.0.0.1:${SYNC_PORT}"
SYNC_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
TOKEN="alice@org1"
ORG="org1"
A_PORT=27801
B_PORT=27802
CUT_PORT=27901
BUDGET_S="${KOMET_E2E_RESUME_BUDGET:-90}"
TMP_DIR="$(mktemp -d /tmp/komet-e2e.XXXXXX)"
LOG_DIR="$TMP_DIR/logs"
mkdir -p "$LOG_DIR"

SYNC_PID=""
A_PID=""
B_PID=""
PROXY_PID=""
STATUS=1

cleanup() {
  for pid in "$SYNC_PID" "$A_PID" "$B_PID" "$PROXY_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  sleep 1
  for pid in "$SYNC_PID" "$A_PID" "$B_PID" "$PROXY_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  if [[ "$STATUS" -ne 0 ]]; then
    echo "--- engine A log (tail) ---"; tail -n 40 "$LOG_DIR/engine-a.log" 2>/dev/null || true
    echo "--- engine B log (tail) ---"; tail -n 40 "$LOG_DIR/engine-b.log" 2>/dev/null || true
    echo "--- sync server log (tail) ---"; tail -n 40 "$LOG_DIR/sync-server.log" 2>/dev/null || true
    for out in "$LOG_DIR"/driver-*.out; do
      [[ -e "$out" ]] || continue
      echo "--- $out ---"; cat "$out"
    done
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
      return 1
    fi
  done
}

start_sync() { # start_sync <data_dir>
  KOMET_SYNC_TOKEN="$SYNC_TOKEN" KOMET_SYNC_PORT="$SYNC_PORT" KOMET_DATA_DIR="$1" \
    RUST_LOG=info "$SYNC" >"$LOG_DIR/sync-server.log" 2>&1 &
  SYNC_PID=$!
  wait_for "sync server /health :$SYNC_PORT" 30 curl -sf -m 3 "$SYNC_URL/health"
  curl -sf -m 3 -H "Authorization: Bearer $SYNC_TOKEN" "$SYNC_URL/env" >/dev/null
}

start_proxy() { # start_proxy <listen_port> <forward_port> <log>
  python3 - "$1" "$2" >"$3" 2>&1 <<'PY' &
import socket, threading, sys

listen_port, forward_port = int(sys.argv[1]), int(sys.argv[2])

def pump(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    try:
        dst.shutdown(socket.SHUT_WR)
    except OSError:
        pass

def handle(client):
    try:
        upstream = socket.create_connection(("127.0.0.1", forward_port))
    except OSError:
        client.close()
        return
    threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pump, args=(upstream, client), daemon=True).start()

server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", listen_port))
server.listen(64)
print(f"proxy listening on {listen_port} -> {forward_port}", flush=True)
while True:
    conn, _ = server.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PY
  PROXY_PID=$!
  wait_for "proxy :$CUT_PORT" 10 bash -c "exec 3<>/dev/tcp/127.0.0.1/$1"
}

start_engines() { # start_engines <a_data_dir> <b_data_dir> [edge_url]
  local edge="${3:-$SYNC_URL}"
  KOMET_DATA_DIR="$1" KOMET_IPC_PORT="$A_PORT" KOMET_DEVICE_NAME="e2e-device-a" \
    KOMET_EDGE_URL="$edge" KOMET_SYNC_TOKEN="$SYNC_TOKEN" \
    KOMET_EDGE_TOKEN="$TOKEN" KOMET_ORG_ID="$ORG" \
    KOMET_TEST_WAKE_FILE="$LOG_DIR/wake-file" \
    KOMET_HARNESS=mock RUST_LOG=info \
    "$KOMET" headless >"$LOG_DIR/engine-a.log" 2>&1 &
  A_PID=$!
  KOMET_DATA_DIR="$2" KOMET_IPC_PORT="$B_PORT" KOMET_DEVICE_NAME="e2e-device-b" \
    KOMET_EDGE_URL="$edge" KOMET_SYNC_TOKEN="$SYNC_TOKEN" \
    KOMET_EDGE_TOKEN="$TOKEN" KOMET_ORG_ID="$ORG" \
    KOMET_TEST_WAKE_FILE="$LOG_DIR/wake-file" \
    KOMET_HARNESS=mock RUST_LOG=info \
    "$KOMET" headless >"$LOG_DIR/engine-b.log" 2>&1 &
  B_PID=$!
  wait_for "engine A ipc :$A_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$A_PORT"
  wait_for "engine B ipc :$B_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$B_PORT"
}

stop_sync() {
  [[ -n "$SYNC_PID" ]] && kill "$SYNC_PID" 2>/dev/null || true
  wait "$SYNC_PID" 2>/dev/null || true
  SYNC_PID=""
}

stop_proxy() {
  [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null || true
  wait "$PROXY_PID" 2>/dev/null || true
  PROXY_PID=""
}

wait_for_marker() { # wait_for_marker <file> <timeout_s>
  local what="$1" timeout="$2"
  wait_for "$what" "$timeout" test -e "$LOG_DIR/$1"
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

# The smoke may only reuse a port it owns: a foreign listener would auth the
# engines against a server with another token and every room join would 401.
if (exec 3<>"/dev/tcp/127.0.0.1/$SYNC_PORT") 2>/dev/null; then
  echo "FAIL: sync port $SYNC_PORT is already in use — refusing to reuse a foreign server" >&2
  echo "      (pick another one with KOMET_E2E_SYNC_PORT)" >&2
  exit 1
fi

# ── scenario runner: a fresh data set per scenario ─────────────────────────────
run_scenario() { # run_scenario <name>
  local name="$1"
  local sync_dir="$TMP_DIR/$name/sync"
  local a_dir="$TMP_DIR/$name/engine-a"
  local b_dir="$TMP_DIR/$name/engine-b"
  mkdir -p "$sync_dir" "$a_dir" "$b_dir"

  case "$name" in
    commands)
      start_sync "$sync_dir" || return 1
      start_engines "$a_dir" "$b_dir" || return 1
      "$DRIVER" "$A_PORT" "$B_PORT" commands || return 1
      ;;

    restart|wake)
      start_sync "$sync_dir" || return 1
      start_engines "$a_dir" "$b_dir" || return 1
      rm -f "$LOG_DIR/ready" "$LOG_DIR/signal" "$LOG_DIR/wake-file"
      KOMET_E2E_READY="$LOG_DIR/ready" KOMET_E2E_SIGNAL="$LOG_DIR/signal" \
        "$DRIVER" "$A_PORT" "$B_PORT" "$name" \
        >"$LOG_DIR/driver-$name.out" 2>&1 &
      local driver_pid=$!
      wait_for_marker "ready" 300 || return 1
      echo "restart: killing the sync server mid-session"
      stop_sync
      sleep 3
      start_sync "$sync_dir" || return 1
      sleep 2
      if [[ "$name" == "wake" ]]; then
        touch "$LOG_DIR/wake-file"
        echo "wake: suspend/resume event injected via $LOG_DIR/wake-file"
        sleep 3
        rm -f "$LOG_DIR/wake-file"
      fi
      touch "$LOG_DIR/signal"
      if ! wait "$driver_pid"; then
        echo "FAIL: $name driver exited nonzero" >&2
        cat "$LOG_DIR/driver-$name.out" >&2
        return 1
      fi
      echo "PASS: scenario $name (driver output in $LOG_DIR/driver-$name.out)"
      if [[ "$name" == "wake" ]]; then
        for log in "$LOG_DIR/engine-a.log" "$LOG_DIR/engine-b.log"; do
          grep -q "wake: test hook fired" "$log" || {
            echo "FAIL: $log lacks the wake-injection line" >&2
            exit 1
          }
          grep -q "system wake; redialing" "$log" || {
            echo "FAIL: $log lacks a wake-triggered redial" >&2
            exit 1
          }
        done
        echo "PASS: both engines redialed on the injected wake event"
      fi
      ;;

    cut)
      start_sync "$sync_dir" || return 1
      start_proxy "$CUT_PORT" "$SYNC_PORT" "$LOG_DIR/proxy.log" || return 1
      start_engines "$a_dir" "$b_dir" "http://127.0.0.1:${CUT_PORT}" || return 1
      rm -f "$LOG_DIR/ready" "$LOG_DIR/signal"
      KOMET_E2E_READY="$LOG_DIR/ready" KOMET_E2E_SIGNAL="$LOG_DIR/signal" \
        "$DRIVER" "$A_PORT" "$B_PORT" cut \
        >"$LOG_DIR/driver-cut.out" 2>&1 &
      local driver_pid=$!
      wait_for_marker "ready" 300 || return 1
      echo "cut: killing the proxy (client-side TCP cut)"
      stop_proxy
      sleep 2
      start_proxy "$CUT_PORT" "$SYNC_PORT" "$LOG_DIR/proxy.log" || return 1
      touch "$LOG_DIR/signal"
      if ! wait "$driver_pid"; then
        echo "FAIL: cut driver exited nonzero" >&2
        cat "$LOG_DIR/driver-cut.out" >&2
        return 1
      fi
      local resume
      resume="$(sed -n 's/^RECONNECT_S=//p' "$LOG_DIR/driver-cut.out")"
      [[ -n "$resume" ]] || { echo "FAIL: driver never reported RECONNECT_S" >&2; return 1; }
      if python3 -c "import sys; sys.exit(0 if float('$resume') < $BUDGET_S else 1)"; then
        echo "PASS: resumed ${resume}s after the cut (budget ${BUDGET_S}s)"
      else
        echo "FAIL: resume took ${resume}s, budget is ${BUDGET_S}s" >&2
        return 1
      fi
      ;;

    checkpoint)
      start_sync "$sync_dir" || return 1
      local chat="e2e-checkpoint"
      local body="$LOG_DIR/checkpoint-body.bin"
      python3 -c 'import os,sys; sys.stdout.buffer.write(os.urandom(1024*1024))' >"$body"
      echo "checkpoint: uploading 1 MB"
      curl -sf -m 30 -X POST \
        -H "Authorization: Bearer $SYNC_TOKEN" \
        -H "x-chat2-frontier;" \
        --data-binary "@$body" \
        "$SYNC_URL/chat2/$chat/checkpoint?seqCovered=0" >/dev/null \
        || { echo "FAIL: checkpoint upload failed" >&2; return 1; }
      # Simulate the cut with a REAL interrupted download: fetch the checkpoint
      # with no Range header and hang up after ~64 KB (the pipe closes, curl
      # sees EPIPE). Resume from the bytes actually received and byte-compare.
      # The hang-up makes curl exit nonzero (EPIPE) — that IS the simulated
      # cut, so the pipeline's status is deliberately not the success signal;
      # the bytes actually received are checked below.
      curl -sf -m 30 \
        -H "Authorization: Bearer $SYNC_TOKEN" \
        "$SYNC_URL/chat2/$chat/checkpoint" 2>/dev/null \
        | head -c 65536 >"$LOG_DIR/part-0.bin" || true
      local received resume
      received="$(stat -c %s "$LOG_DIR/part-0.bin" 2>/dev/null \
        || stat -f %z "$LOG_DIR/part-0.bin")" \
        || { echo "FAIL: cannot size the partial download" >&2; return 1; }
      if [[ "$received" -le 0 || "$received" -ge "$(stat -c %s "$body" 2>/dev/null || stat -f %z "$body")" ]]; then
        echo "FAIL: partial download got $received bytes — the transfer was not cut" >&2
        return 1
      fi
      # Serve the remainder from the byte we actually stopped at.
      resume="$received"
      curl -sf -m 30 \
        -H "Authorization: Bearer $SYNC_TOKEN" \
        -r "$resume-" \
        "$SYNC_URL/chat2/$chat/checkpoint" >"$LOG_DIR/part-1.bin" \
        || { echo "FAIL: Range resume request failed" >&2; return 1; }
      cat "$LOG_DIR/part-0.bin" "$LOG_DIR/part-1.bin" >"$LOG_DIR/part-joined.bin" \
        || { echo "FAIL: cannot join the downloaded parts" >&2; return 1; }
      cmp "$body" "$LOG_DIR/part-joined.bin" \
        || { echo "FAIL: Range-resumed checkpoint does not match the original" >&2; return 1; }
      echo "PASS: 1 MB checkpoint resumed by Range from byte $resume matches byte for byte"
      ;;

    *) echo "FAIL: unknown scenario $name" >&2; exit 1 ;;
  esac
}

SCENARIOS=("$SCENARIO")
if [[ "$SCENARIO" == "all" ]]; then SCENARIOS=(commands restart wake cut checkpoint); fi

FAILED=0
for name in "${SCENARIOS[@]}"; do
  echo "── scenario: $name ──────────────────────────────────────────────────────"
  if ! run_scenario "$name"; then
    echo "FAIL: scenario $name" >&2
    FAILED=1
    STATUS=1
    # Raze the processes before the next scenario; the trap cleans up.
    for pid in "$SYNC_PID" "$A_PID" "$B_PID" "$PROXY_PID"; do
      [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
    done
    SYNC_PID=""; A_PID=""; B_PID=""; PROXY_PID=""
    continue
  fi
  for pid in "$SYNC_PID" "$A_PID" "$B_PID" "$PROXY_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  SYNC_PID=""; A_PID=""; B_PID=""; PROXY_PID=""
done

STATUS=$FAILED
exit "$STATUS"
