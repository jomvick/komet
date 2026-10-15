#!/bin/sh
# Fake agy with a failing `models` probe (logged-out / broken CLI, exit 1
# with a login hint on stderr); every other invocation delegates to the
# happy-path fake so run-path tests stay untouched.
if [ "$1" = "models" ]; then
    echo "error: authentication required, run \`agy login\` to sign in" >&2
    exit 1
fi
exec "$(dirname "$0")/fake-agy.sh" "$@"
