#!/bin/sh
# Old-CLI variant: rejects the modern `list_models` subtype and only answers the
# SDK-style `supported_models`, exercising the fallback path in
# ClaudeHarness::discover_live_models.
CLAUDE_LEGACY_MODELS=1 exec "$(dirname "$0")/fake-claude.sh" "$@"
