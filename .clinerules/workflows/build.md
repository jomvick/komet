# Build Komet

Build the full Komet workspace in release mode and report the result.

## Steps

1. Run `cargo build --release --workspace` from the project root.
2. If the build fails, analyze the compiler errors, identify the root cause, and propose a fix.
3. If the build succeeds, confirm which binaries were produced under `target/release/`.
