#!/bin/sh
CLAUDE_FAIL_MODELS=1 exec "$(dirname "$0")/fake-claude.sh" "$@"
