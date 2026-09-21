#!/bin/sh
CODEX_FAIL_MODEL_LIST=1 exec "$(dirname "$0")/fake-codex.sh" "$@"
