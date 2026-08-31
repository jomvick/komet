#!/usr/bin/env python3
"""Probe the real `claude` CLI's stream-json wire for komet's slash-commands
work (see docs/research/slash-commands-inventory.md). Answers the two things
that inventory flags as unverifiable from docs alone:

  1. What does `initialize`'s `commands` list actually contain in
     `--print --input-format stream-json` mode — full built-in set, or a
     smaller non-interactive subset? (dumps the RAW response, not just names)
  2. What stdout frame(s) does the CLI emit when a built-in like `/compact` or
     `/clear` is sent as a literal first line of a prompt over stdin? Does
     `komet_harness::claude::normalize::Normalizer` need a new arm?

It then probes the six control_request verbs docs/research/harness.md already
documents on this same channel (`set_permission_mode`, `set_model`,
`rewind_files`, `mcp_reconnect`, `get_context_usage`, `stop_task`) with
best-guess payloads, so we see real accept/reject shapes instead of guessing
them into driver code.

COST WARNING: sending `/compact` and `/clear` as real prompt turns spends a
small amount of real API usage (compaction summarizes an empty/tiny
conversation; harmless, but not free). Everything else is control-channel
only and costs nothing.

Usage: python3 scripts/probe-claude-commands.py
Full raw log: /tmp/komet_probe/frames.log (every line, unabridged)
"""

import json
import os
import shutil
import subprocess
import sys
import threading
import time

LOG_DIR = "/tmp/komet_probe"
os.makedirs(LOG_DIR, exist_ok=True)
LOG = open(f"{LOG_DIR}/frames.log", "a", buffering=1)
T0 = time.monotonic()


def log(direction, raw_or_obj):
    t = time.monotonic() - T0
    line = raw_or_obj if isinstance(raw_or_obj, str) else json.dumps(raw_or_obj, separators=(",", ":"))
    print(f"{t:8.3f} {direction} {line}", file=LOG)


def banner(msg):
    print(f"\n=== {msg} ===")
    log("== phase ==", msg)


claude = shutil.which("claude")
if not claude:
    print("claude CLI not found on PATH — set it up first, or point this script at your binary.")
    sys.exit(1)

proc = subprocess.Popen(
    [claude, "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(f"{LOG_DIR}/stderr.log", "ab"),
    text=True,
    bufsize=1,
    env={**os.environ},
)

lock = threading.Lock()
frames = []  # (t, dict) for every parsed stdout line since the last drain


def send(obj):
    msg = json.dumps(obj)
    log(">>", msg)
    proc.stdin.write(msg + "\n")
    proc.stdin.flush()


def send_user_text(text):
    send({"type": "user", "message": {"role": "user", "content": text}, "parent_tool_use_id": None})


def reader():
    for raw in proc.stdout:
        raw = raw.strip()
        if not raw:
            continue
        try:
            obj = json.loads(raw)
        except json.JSONDecodeError:
            log("!! non-json", raw[:300])
            continue
        log("<<", obj)
        with lock:
            frames.append((time.monotonic() - T0, obj))
    log("!!", "stdout EOF")


threading.Thread(target=reader, daemon=True).start()


def drain(seconds, note=""):
    """Collect + print every frame that arrives in the next `seconds`."""
    deadline = time.monotonic() + seconds
    seen_from = len(frames)
    while time.monotonic() < deadline:
        time.sleep(0.2)
    with lock:
        batch = frames[seen_from:]
    print(f"  ({note}) {len(batch)} frame(s):")
    for _, obj in batch:
        kind = obj.get("type")
        sub = obj.get("subtype")
        print(f"    type={kind} subtype={sub}  keys={sorted(obj.keys())}")
    return [o for _, o in batch]


def control_request(request_id, request_body, wait_s=8):
    send({"type": "control_request", "request_id": request_id, "request": request_body})
    deadline = time.monotonic() + wait_s
    while time.monotonic() < deadline:
        with lock:
            for _, obj in frames:
                if obj.get("type") == "control_response" and (obj.get("response") or {}).get("request_id") == request_id:
                    return obj
        time.sleep(0.1)
    return None


# ---- Phase 1: initialize — dump the RAW commands list ----------------------
banner("1. initialize — raw commands list")
resp = control_request("probe-init", {"subtype": "initialize"}, wait_s=15)
if resp is None:
    print("  initialize never answered — CLI version/flags problem, stopping here.")
    proc.terminate()
    sys.exit(1)
commands = ((resp.get("response") or {}).get("commands")) or []
print(f"  {len(commands)} commands advertised. First 20 raw entries:")
for c in commands[:20]:
    print(f"    {json.dumps(c)}")
print(f"  Full list dumped to {LOG_DIR}/frames.log — grep for 'probe-init'.")
print("  MANUAL CHECK: open a plain interactive `claude` (no --print) in another terminal,")
print("  type `/` and compare its menu against the names above. Anything in the TUI menu")
print("  but missing here is non-interactive-filtered — note it in the inventory.")

# ---- Phase 2: /compact and /clear as real prompt turns ---------------------
banner("2. sending /compact as a prompt line (spends a small real turn)")
send_user_text("/compact")
compact_frames = drain(30, "after /compact")

banner("3. sending /clear as a prompt line")
send_user_text("/clear")
clear_frames = drain(15, "after /clear")

print("\n  Look at the subtypes above. If nothing but the usual assistant/result frames")
print("  showed up, the CLI is treating '/compact'/'/clear' as ordinary chat text, NOT")
print("  as the built-in — komet-side sending won't work as-is and these need a different")
print("  channel (or a CLI flag we're missing). If a NEW `system` subtype appeared")
print("  (e.g. compact_boundary), that's the arm Normalizer::normalize() needs to add.")

# ---- Phase 4: the six documented control_request verbs ---------------------
banner("4. probing the six control_request verbs from docs/research/harness.md")
probes = [
    ("probe-get-context", {"subtype": "get_context_usage"}),
    ("probe-set-perm", {"subtype": "set_permission_mode", "mode": "default"}),
    ("probe-set-model", {"subtype": "set_model", "model": "sonnet"}),
    ("probe-mcp-status", {"subtype": "mcp_reconnect", "action": "status"}),
    ("probe-rewind", {"subtype": "rewind_files", "checkpoint": "latest"}),
    ("probe-stop-task", {"subtype": "stop_task", "task_id": "nonexistent"}),
]
for request_id, body in probes:
    resp = control_request(request_id, body, wait_s=8)
    if resp is None:
        print(f"  {body['subtype']:20s} -> NO RESPONSE within timeout (unknown/unsupported subtype?)")
    else:
        print(f"  {body['subtype']:20s} -> {json.dumps(resp.get('response'))}")

print(f"\nFull unabridged log: {LOG_DIR}/frames.log")
print("Paste that file (or the interesting parts) back so the driver code can be written")
print("against real shapes instead of guesses.")

proc.stdin.close()
time.sleep(1)
proc.terminate()
