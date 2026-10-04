# Run Tests

Run the full test suite for the Komet workspace and summarize the results.

## Steps

1. Run `cargo test --workspace` from the project root.
2. Parse the output and list:
   - Total tests passed
   - Any failures with their crate name and test name
   - Any compilation errors before tests ran
3. If there are failures, investigate the failing tests and suggest fixes.
