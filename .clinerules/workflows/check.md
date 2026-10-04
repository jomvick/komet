# Check & Lint

Run a full lint and type-check pass on the Komet workspace.

## Steps

1. Run `cargo check --workspace` to verify all crates compile without errors.
2. Run `cargo clippy --workspace -- -D warnings` to catch style and logic issues.
3. Summarize all warnings and errors found.
4. For each issue, propose the minimal fix needed to resolve it.
