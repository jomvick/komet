#!/bin/sh
# Fake komet cursor shim for komet-harness tests: speaks the shim's JSONL
# protocol (see crates/harness/src/cursor/shim.mjs) without node or the SDK.
# Driven by crates/harness/tests/cursor.rs.

emit() { printf '%s\n' "$1"; }

# Models mode (argv, no stdin protocol): one catalog frame, real 1.0.28
# shapes — parameterized Auto + its bare `default` alias twin (skipped by the
# harness) + a plain model.
if [ "$1" = "login" ]; then
  store="$2"
  if [ -z "$store" ]; then
    emit '{"ev":"fatal","message":"login mode needs a store path"}'
    exit 1
  fi
  emit '{"ev":"auth-url","url":"https://cursor.com/login?test=1"}'
  mkdir -p "$(dirname "$store")"
  printf '%s\n' '{"apiKey":"test-cursor-key","email":"dev@example.com"}' > "$store"
  emit '{"ev":"logged-in","email":"dev@example.com"}'
  exit 0
fi

if [ "$1" = "models" ]; then
  emit '{"ev":"models","items":[{"id":"auto-smart","displayName":"Auto","parameters":[{"id":"optimize_for","displayName":"Optimize For","values":[{"value":"intelligence","displayName":"Intelligence"},{"value":"balanced","displayName":"Balance"},{"value":"cost","displayName":"Cost"}]}],"variants":[{"params":[{"id":"optimize_for","value":"balanced"}],"displayName":"Auto","isDefault":true}]},{"id":"default","displayName":"Auto","aliases":["auto"]},{"id":"composer-2.5","displayName":"Composer 2.5","description":"Cursor native","parameters":[{"id":"fast","values":[{"value":"false"},{"value":"true"}]}]}]}'
  exit 0
fi

read -r first || exit 1
case "$first" in
*'"op":"run"'*) ;;
*) emit '{"ev":"fatal","message":"expected op run first"}'; exit 1 ;;
esac

case "$first" in

*scenario:happy*)
  emit '{"ev":"ready","agentId":"agent-1","model":"composer-2.5"}'
  emit '{"ev":"thinking","text":"planning"}'
  emit '{"ev":"text","text":"Hello from cursor"}'
  emit '{"ev":"tool","phase":"start","id":"c1","name":"shell","args":{"command":"ls -la"}}'
  emit '{"ev":"tool","phase":"end","id":"c1","name":"shell","args":{"command":"ls -la"},"error":false}'
  # A spawned subagent: the task chip on the parent feed, its interior tagged.
  emit '{"ev":"tool","phase":"start","id":"task1","name":"task","args":{"description":"scan repo"}}'
  emit '{"ev":"text","text":"sub scanning","parent":"task1"}'
  emit '{"ev":"tool","phase":"start","id":"s1","name":"grep","args":{"pattern":"todo"},"parent":"task1"}'
  emit '{"ev":"tool","phase":"end","id":"s1","name":"grep","args":{"pattern":"todo"},"error":false,"parent":"task1"}'
  emit '{"ev":"tool","phase":"end","id":"task1","name":"task","args":{"description":"scan repo"},"error":false}'
  # Unknown frame kinds must be tolerated.
  emit '{"ev":"someNewThing","x":1}'
  emit '{"ev":"usage","input":11,"output":5}'
  emit '{"ev":"turn","status":"finished"}'
  # Parked: wait for a follow-up or stdin EOF.
  read -r next || exit 0
  case "$next" in
  *'"op":"user"'*)
    emit '{"ev":"text","text":"second turn"}'
    emit '{"ev":"turn","status":"finished"}'
    ;;
  esac
  exit 0
  ;;

*scenario:interrupt*)
  emit '{"ev":"ready","agentId":"agent-int","model":"auto"}'
  emit '{"ev":"text","text":"working"}'
  read -r msg || exit 0
  case "$msg" in
  *'"op":"interrupt"'*)
    emit '{"ev":"turn","status":"cancelled"}'
    ;;
  esac
  exit 0
  ;;

*scenario:fatal*)
  emit '{"ev":"fatal","message":"Cursor SDK is not authenticated (its login is separate from `cursor-agent login`): set CURSOR_API_KEY from cursor.com/settings, then retry."}'
  exit 1
  ;;

*scenario:crash*)
  emit '{"ev":"ready","agentId":"agent-c","model":"auto"}'
  echo "shim exploded" >&2
  exit 3
  ;;

*)
  emit '{"ev":"fatal","message":"unknown scenario"}'
  exit 1
  ;;
esac
