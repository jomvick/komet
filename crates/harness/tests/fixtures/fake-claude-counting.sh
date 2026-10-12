#!/bin/sh
# Spawn-counting wrapper around fake-claude.sh for the models() single-flight
# regression test: appends one line per CLI spawn to $KOMET_CLAUDE_PROBE_COUNT,
# then delegates so probe behavior is identical to the happy-path fake.
if [ -n "$KOMET_CLAUDE_PROBE_COUNT" ]; then
    printf 'spawn\n' >> "$KOMET_CLAUDE_PROBE_COUNT"
fi
exec "$(dirname "$0")/fake-claude.sh" "$@"
