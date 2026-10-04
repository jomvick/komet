# Explore Codebase

Give a structured tour of the Komet codebase relevant to a given topic or crate.

## Instructions

The user will specify a topic, crate name, or question after the slash command
(e.g. `/explore sync engine`).

## Steps

1. Read `ARCHITECTURE.md` for the overall design context.
2. Navigate to the relevant crates under `crates/` or `apps/`.
3. Produce a clear summary:
   - Purpose of the crate / module
   - Key types and their responsibilities
   - Main data-flow or call paths
   - Any notable design patterns or TODOs
4. Reference specific files and line numbers so the user can jump directly to the code.
