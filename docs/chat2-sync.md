# Session synchronization protocol

> Current operator setup, migration commands, and s2 compatibility policy live in
> [`sync.md`](sync.md). This page is a short protocol map for maintainers.

Komet stores each session as a Loro document. A host engine persists snapshots
and journals locally, then exchanges document updates and durable commands with
the self-hosted `komet-sync-server`. Clients reconnect and resume from local
room state; command ownership stays with the device hosting the session.

The current implementation is in `crates/doc` (schema, projection, rebuild),
`crates/sync` (room client and local snapshots), `crates/engine/src/chat2_host.rs`
and `crates/sync-server` (room persistence and blob routes).

The engine still reads and migrates s2 sessions for upgrade compatibility. See
[`sync.md`](sync.md#s2-migration-and-compatibility) before changing those readers.
