# Workspace registry protocol

> Current operator setup and migration commands live in [`sync.md`](sync.md).

The synchronized workspace registry contains device, project, chat, session
status, and checkout summary rows. The self-hosted Rust server stores the
current row state in SQLite and applies validated operations with deterministic
conflict ordering. Transcript bodies remain in per-chat Loro documents.

Implementation: `crates/doc/src/registry.rs` defines the shared row and
operation model; `crates/sync/src/registry.rs` handles the client protocol;
`crates/sync-server/src/registry.rs` persists and merges the server state.
