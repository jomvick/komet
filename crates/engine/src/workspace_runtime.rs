//! One workspace runtime: the docs store plus the two hosts that read and write it.
//!
//! The local workspace and every remote environment are the same thing built from this
//! constructor — same docs, same registry, same room clients — differing only in store
//! root, identity and edge. Device-scoped services (sessions, terminals, repos, agent
//! credentials) stay outside: they describe this machine, not a workspace.

use std::path::Path;
use std::sync::Arc;

use komet_proto::HarnessId;
use komet_sync::DocsStore;

use crate::EngineError;
use crate::doc_host::{DocHost, DocHostConfig, EdgeConfig};
use crate::workspace_host::{WorkspaceHost, WorkspaceHostConfig};

pub struct WorkspaceRuntime {
    pub store: Arc<DocsStore>,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
}

impl WorkspaceRuntime {
    /// Open `store_root`, then wire a doc host and a workspace host onto it.
    ///
    /// The sessions engine is deliberately **not** attached here: the local workspace
    /// attaches it (it executes chats it hosts), while a remote environment never does.
    pub fn open(
        store_root: &Path,
        device_id: &str,
        device_name: String,
        org_id: &str,
        user_id: &str,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let store = Arc::new(DocsStore::open(store_root)?);
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.to_string(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: device_id.to_string(),
                device_name,
                platform: std::env::consts::OS.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                edge,
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        Ok(Self {
            store,
            doc_host,
            workspace,
        })
    }
}
