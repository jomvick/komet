# Format Code

Format the entire Komet workspace using rustfmt.

## Steps

1. Run `cargo fmt --all` from the project root.
2. Run `git diff --stat` to show which files were reformatted.
3. If no files changed, confirm the codebase is already clean.
