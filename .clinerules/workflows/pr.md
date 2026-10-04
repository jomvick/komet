# Prepare a Pull Request

Prepare a clean pull request for the current changes.

## Steps

1. Run `git status` and `git diff --stat HEAD` to see what has changed.
2. Run `cargo check --workspace` and `cargo clippy --workspace -- -D warnings` — fix any errors before continuing.
3. Run `cargo test --workspace` — all tests must pass.
4. Run `cargo fmt --all` to format the code.
5. Summarize the changes in a concise PR description:
   - **What changed** (which crates / files)
   - **Why** (motivation / context)
   - **How to test** (manual steps or automated tests that cover it)
6. Suggest a clear, conventional commit message for the changes.
