#!/bin/sh
# Fake Cline ACP server for komet-harness tests.
#
# Mimes the real `cline --acp` wire (verified live, cline 3.0.61 / agent
# 3.0.62): initialize answers protocolVersion 1 with loadSession, then
# session/new advertises BOTH a `provider` select (cline / cline-pass /
# openai-codex) AND the real `model` select — both with the `model`
# category, provider FIRST. Discovery must pick the option actually named
# `model`, not the first category match (else the auth providers would show
# up as the model list). Driven by crates/harness/tests/acp.rs.
#
# SCENARIO env selects the advertised state:
#   two-tier   — provider select first, then the model select (the real wire)
#   model-only — only the model select (existing-agent behaviour)
#   unauthed   — session/new rejects with -32000 Authentication required
#                (an unsigned-in `cline`), handshake must fail loudly

# The spec launches with `cline --acp`; refuse anything else.
[ "$1" = "--acp" ] || exit 1

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
case "$line" in *'"method":"initialize"'*) ;; *) exit 1 ;; esac
case "$line" in *'"protocolVersion":1'*) ;; *) exit 1 ;; esac
case "$line" in *'"name":"komet"'*) ;; *) exit 1 ;; esac
case "$line" in *'"readTextFile":false'*) ;; *) exit 1 ;; esac
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}"

# ---- session new -----------------------------------------------------------
read -r line || exit 1
SID="s-cline"
case "$line" in *'"method":"session/new"'*) ;; *) exit 1 ;; esac
if [ "${SCENARIO:-two-tier}" = "unauthed" ]; then
  emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32000,\"message\":\"Authentication required: Call authenticate before starting a session\"}}"
elif [ "${SCENARIO:-two-tier}" = "model-only" ]; then
  emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"$SID\",\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"m/1\",\"options\":[{\"value\":\"a/b\",\"name\":\"A B\"}]}]}}"
else
  models="{\"value\":\"some/model-001\",\"name\":\"Model 001\"}"
  i=2
  while [ "$i" -le 301 ]; do
    padded=$(printf '%03d' "$i")
    models="$models,{\"value\":\"some/model-$padded\",\"name\":\"Model $padded\"}"
    i=$((i + 1))
  done
  emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"$SID\",\"configOptions\":[{\"id\":\"provider\",\"name\":\"Provider\",\"description\":\"The authentication provider to use\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"cline\",\"options\":[{\"value\":\"cline\",\"name\":\"Cline Usage-Billing\"},{\"value\":\"cline-pass\",\"name\":\"ClinePass\"},{\"value\":\"openai-codex\",\"name\":\"OpenAI ChatGPT Subscription\"}]},{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"some/model-001\",\"options\":[$models]}]}}"
fi
