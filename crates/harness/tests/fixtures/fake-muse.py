#!/usr/bin/env python3
import json
import os
import sys


scenario = os.environ.get("MUSE_TEST_SCENARIO", "complete")
log_path = os.environ.get("MUSE_TEST_LOG")
session_id = None
turn_id = None
turn_count = 0

if log_path:
    with open(log_path + ".args", "w", encoding="utf-8") as log:
        json.dump(sys.argv[1:], log)


def record(frame):
    if log_path:
        with open(log_path, "a", encoding="utf-8") as log:
            log.write(json.dumps(frame) + "\n")


def write(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def respond(frame, result):
    write({"jsonrpc": "2.0", "id": frame["id"], "result": result})


def notify(method, params):
    write({"jsonrpc": "2.0", "method": method, "params": params})


def read():
    line = sys.stdin.readline()
    if not line:
        return None
    frame = json.loads(line)
    record(frame)
    return frame


def finish(terminal="completed"):
    notify("turn/completed", {
        "sessionId": session_id,
        "turnId": turn_id,
        "terminal": terminal,
    })


def user_message():
    notify("item/started", {
        "sessionId": session_id,
        "turnId": turn_id,
        "item": {"kind": "userMessage", "itemId": "user-" + str(turn_id)},
    })


for frame in iter(read, None):
    method = frame.get("method")
    params = frame.get("params", {})
    if method == "initialize":
        respond(frame, {
            "schema": {"version": 1},
            "grantedCapabilities": [],
        })
    elif method == "model/list":
        respond(frame, {"models": [{
            "providerId": "meta",
            "modelId": "muse-spark-test",
            "displayLabel": "Muse Spark Test",
            "isDefault": True,
            "variants": ["low", "high"],
        }]})
    elif method == "session/start":
        session_id = params["sessionId"]
        respond(frame, {"session": {
            "sessionId": session_id,
            "modelId": params.get("modelId", "muse-spark-test"),
        }})
    elif method == "session/resume":
        session_id = params["sessionId"]
        respond(frame, {"session": {
            "sessionId": session_id,
            "modelId": "muse-spark-test",
        }})
    elif method == "session/setApprovalMode":
        respond(frame, {})
    elif method == "turn/start":
        turn_count += 1
        if turn_count == 1:
            turn_id = params["commandId"]
        respond(frame, {
            "turnId": turn_id,
            "disposition": "started" if turn_count == 1 else "steered",
        })
        user_message()
        if scenario == "permission" and turn_count == 1:
            notify("approval/requested", {
                "sessionId": session_id,
                "approvalId": "approval-1",
                "currentRequirementId": {"approvalId": "approval-1"},
                "toolName": "bash",
                "rawArgs": "{\"command\":\"touch marker\"}",
                "availableChoices": [
                    {"decision": "approved", "scope": "once", "choiceId": "allow-once"},
                    {"decision": "abort", "choiceId": "deny"},
                ],
            })
            decision = read()
            if decision is not None and decision.get("method") == "approval/decide":
                respond(decision, {})
            notify("item/delta", {
                "sessionId": session_id,
                "turnId": turn_id,
                "itemId": "assistant-1",
                "delta": "permission handled",
            })
            finish()
        elif scenario == "cancel":
            while True:
                interrupted = read()
                if interrupted is None:
                    break
                if interrupted.get("method") == "turn/interrupt":
                    respond(interrupted, {})
                    finish("cancelled")
                    break
        elif scenario == "steer" and turn_count == 1:
            notify("item/delta", {
                "sessionId": session_id,
                "turnId": turn_id,
                "itemId": "assistant-1",
                "delta": "first",
            })
        else:
            notify("item/delta", {
                "sessionId": session_id,
                "turnId": turn_id,
                "itemId": "assistant-" + str(turn_count),
                "delta": "Muse response",
            })
            finish()
    elif method == "turn/interrupt":
        respond(frame, {})
        finish("cancelled")
