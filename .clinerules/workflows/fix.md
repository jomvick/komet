# Fix a Bug

Diagnose and fix a reported bug in Komet.

## Instructions

The user will describe the bug after the slash command
(e.g. `/fix sync drops connection after 30s`).

## Steps

1. Reproduce the issue by reading relevant code paths, logs, or test output shared by the user.
2. Run `cargo check --workspace` to confirm the current compilation state.
3. Identify the root cause — search the codebase for the relevant code.
4. Implement the minimal fix needed.
5. Add or update a test that would catch this regression.
6. Run `cargo test --workspace` to confirm all tests pass after the fix.
7. Summarize what was wrong and what was changed.
