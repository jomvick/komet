//! komet-sync — Loro room client (loro-protocol over WebSocket against the TS edge),
//! ephemeral presence, and the local `DocsStore` (SQLite snapshots + processed-command ledger).
//!
//! - [`RoomClient`]: joins a SessionRoom DO room (`wss://…/session/{chatId}/ws?token=`),
//!   backfills via version-vector diff, pushes local commits, imports remote updates,
//!   reassembles/produces fragments, relays `%EPH` presence, and reconnects with
//!   exponential backoff. Wire format is the official `loro-protocol` crate — byte-identical
//!   to the npm package the edge imports.
//! - [`DocsStore`]: snapshot persistence (the doc IS the outbox — commands + user entries
//!   flush immediately) and the processed-command ledger with mark-BEFORE-execute semantics.

pub mod chat_client;
pub mod chat_frames;
pub mod registry;
mod room;
mod store;
pub mod wake;

pub use chat_client::{
    ChatClient, ChatDocSink, ChatEvent, ChatStatsSnapshot, ChatTuning, CheckpointFetcher,
};
pub use registry::{RegistryClient, RegistryEvent, RegistryTuning};
pub use room::{
    RoomClient, RoomEvent, RoomStatsSnapshot, RoomTuning, StaticUrl, SyncError, UrlProvider,
};
pub use store::{DocsStore, StoreError};
