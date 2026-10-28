//! Remote environments: one [`WorkspaceRuntime`] per sync server, kept live in
//! the background for the whole session.
//!
//! Isolation is the network boundary: each environment has its own docs store, its
//! own registry room and its own bearer, so no code path can mix its rows with the
//! local workspace's. The laptop is a controller in a remote environment, never a
//! device: runtimes here are opened with `register_device = false`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use komet_proto::{
    Environment, HarnessId, Namespace, PAIR_VERSION, PairPayload, decode_pair_link,
    encode_pair_link,
};
use komet_rpc::{LinkCache, LinkCacheConfig, StaticToken};
use tokio::sync::watch;

use crate::doc_host::EdgeConfig;
use crate::environments::{AddOutcome, EnvStoreError, EnvironmentStore, LOCAL_ENV_ID};
use crate::workspace_runtime::WorkspaceRuntime;

/// Label of the machine's own workspace in the switcher. The UI may translate it.
const LOCAL_LABEL: &str = "This computer";

/// Why `AddEnvironment` refused a link. Codes are stable: the UI branches on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvAddError {
    /// Not a `komet://pair/` link, or a malformed one.
    Link(komet_proto::EnvError),
    /// DNS, TLS, timeout, or a non-`/env` answer.
    Unreachable(String),
    /// The server rejected the token.
    Unauthorized,
    VersionMismatch {
        server: u32,
        client: u32,
    },
    /// The server is the one this profile is already synced to.
    AlreadySyncedHere,
    Store(EnvStoreError),
    Runtime(String),
}

impl EnvAddError {
    /// Short, stable wire code (`RpcError::Failed` carries it verbatim).
    pub fn code(&self) -> String {
        match self {
            Self::Link(err) => format!("invalid_link: {err}"),
            Self::Unreachable(detail) => format!("unreachable: {detail}"),
            Self::Unauthorized => "unauthorized".into(),
            Self::VersionMismatch { server, client } => {
                format!("version_mismatch: server {server}, client {client}")
            }
            Self::AlreadySyncedHere => "already_synced_here".into(),
            Self::Store(err) => format!("store: {err}"),
            Self::Runtime(detail) => format!("runtime: {detail}"),
        }
    }
}

impl std::fmt::Display for EnvAddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}

impl std::error::Error for EnvAddError {}

impl From<komet_proto::EnvError> for EnvAddError {
    fn from(err: komet_proto::EnvError) -> Self {
        Self::Link(err)
    }
}

impl From<EnvStoreError> for EnvAddError {
    fn from(err: EnvStoreError) -> Self {
        Self::Store(err)
    }
}

/// One live environment.
pub struct EnvHandle {
    pub env: Environment,
    pub runtime: WorkspaceRuntime,
    /// Device routing *inside* this environment (its own relay namespace).
    pub links: Arc<LinkCache>,
}

/// The environment list and its live runtimes. Interior-mutable: adding or
/// removing happens while other connections are being served.
pub struct EnvHosts {
    data_dir: PathBuf,
    device_id: String,
    device_name: String,
    default_harness: HarnessId,
    /// Namespace of the profile this engine serves; adding it is refused.
    own: Namespace,
    store: RwLock<EnvironmentStore>,
    /// The stored file could not be read: serve "no environments" and refuse
    /// every write, so repairing the file is the only way to change the list.
    read_only: bool,
    entries: RwLock<Vec<EnvHandle>>,
    listing: watch::Sender<Vec<Environment>>,
}

/// A store whose file cannot be written back until the user repairs it.
const STORE_UNREADABLE: &str =
    "environments.json is unreadable; repair or delete it, then restart Komet";

impl EnvHosts {
    /// Load `{data_dir}/environments.json` and open every stored environment.
    /// An unreadable file boots with no environments (never blocking the local
    /// workspace) and stays read-only, because saving would erase its tokens.
    pub fn boot(
        data_dir: &Path,
        device_id: &str,
        device_name: String,
        default_harness: HarnessId,
        own: Namespace,
    ) -> Self {
        let (store, read_only) = match EnvironmentStore::load(data_dir) {
            Ok(store) => (store, false),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "environments file unreadable; remote environments are disabled until it is \
                     repaired"
                );
                (EnvironmentStore::empty(data_dir), true)
            }
        };
        let hosts = Self {
            data_dir: data_dir.to_path_buf(),
            device_id: device_id.to_string(),
            device_name,
            default_harness,
            own,
            store: RwLock::new(store),
            read_only,
            entries: RwLock::new(Vec::new()),
            listing: watch::channel(Vec::new()).0,
        };
        let stored = hosts
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec();
        for env in stored {
            if env.namespace.is_none() {
                tracing::warn!(env = %env.id, "environment has no namespace; skipping until re-added");
                continue;
            }
            match hosts.open_runtime(&env) {
                Ok(handle) => hosts
                    .entries
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(handle),
                Err(err) => {
                    tracing::warn!(env = %env.id, error = %err, "environment open failed; skipping")
                }
            }
        }
        hosts.publish();
        hosts
    }

    /// The switcher's list: the local workspace first, then every environment.
    /// Tokens are redacted — no RPC reply ever carries a bearer.
    pub fn list(&self) -> Vec<Environment> {
        let mut list = vec![Environment {
            id: LOCAL_ENV_ID.to_string(),
            label: LOCAL_LABEL.to_string(),
            url: String::new(),
            token: String::new(),
            namespace: Some(self.own.clone()),
            added_at_ms: 0,
        }];
        list.extend(
            self.store
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .list()
                .iter()
                .map(redacted),
        );
        list
    }

    pub fn watch(&self) -> watch::Receiver<Vec<Environment>> {
        self.listing.subscribe()
    }

    /// Re-encode one environment's pairing link, bearer included, so the user can
    /// paste it on another controller. Every other reply is redacted — this is the
    /// one that is deliberately not, so it stays on the local connection and is
    /// never logged or embedded in an error message.
    pub fn pair_link(&self, id: &str) -> Option<String> {
        let env = self
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()?;
        Some(encode_pair_link(&PairPayload {
            v: PAIR_VERSION,
            label: env.label,
            url: env.url,
            token: env.token,
        }))
    }

    /// The live runtime for `id`, or `None` when it is unknown or failed to open.
    pub fn runtime(&self, id: &str) -> Option<WorkspaceRuntime> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|handle| handle.env.id == id)
            .map(|handle| handle.runtime.clone())
    }

    /// The device-routing cache for `id` (relay links inside that environment).
    pub fn links(&self, id: &str) -> Option<Arc<LinkCache>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|handle| handle.env.id == id)
            .map(|handle| handle.links.clone())
    }

    /// Decode a link, verify the server answers with that token, and start using
    /// it. Idempotent on `(url, token)`: pairing twice returns the same id.
    pub async fn add(&self, link: &str) -> Result<Environment, EnvAddError> {
        if self.read_only {
            return Err(EnvAddError::Store(EnvStoreError::Io(
                STORE_UNREADABLE.into(),
            )));
        }
        let payload = decode_pair_link(link)?;
        let probe = probe_server(&payload.url, &payload.token).await?;
        if probe.org_id == self.own.org_id && probe.user_id == self.own.user_id {
            return Err(EnvAddError::AlreadySyncedHere);
        }
        let namespace = Namespace {
            org_id: probe.org_id,
            user_id: probe.user_id,
        };
        {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = store
                .list()
                .iter()
                .find(|env| env.url == payload.url && env.token == payload.token)
            {
                return Ok(redacted(existing));
            }
        }
        let env = Environment {
            id: uuid::Uuid::new_v4().to_string(),
            label: payload.label,
            url: payload.url,
            token: payload.token,
            namespace: Some(namespace),
            added_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let handle = self.open_runtime(&env)?;
        // Scoped: the write guard must not live across the teardown await below.
        let outcome: Result<Option<Environment>, EnvAddError> = {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            let mut next = store.clone();
            match next.upsert(env.clone()) {
                // A concurrent add stored the same link first: keep its runtime,
                // drop ours instead of leaving a second room nobody can remove.
                AddOutcome::Existing(id) => Ok(Some(Environment { id, ..env.clone() })),
                AddOutcome::Added => match next.save() {
                    Ok(()) => {
                        *store = next;
                        Ok(None)
                    }
                    Err(err) => Err(EnvAddError::Store(EnvStoreError::Io(format!(
                        "{}: {err}",
                        self.data_dir.join("environments.json").display()
                    )))),
                },
            }
        };
        match outcome {
            Ok(None) => {}
            Ok(Some(existing)) => {
                teardown(&handle).await;
                return Ok(redacted(&existing));
            }
            Err(err) => {
                teardown(&handle).await;
                return Err(err);
            }
        }
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle);
        self.publish();
        Ok(redacted(&env))
    }

    /// Forget an environment and stop its clients. `purge` also deletes its docs
    /// mirror: the mirror is the last copy when the server is gone, so deleting it
    /// is always explicit.
    pub async fn remove(&self, id: &str, purge: bool) -> Result<(), EnvStoreError> {
        if self.read_only {
            return Err(EnvStoreError::Io(STORE_UNREADABLE.into()));
        }
        let handle = {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            let mut next = store.clone();
            next.remove(id)?;
            next.save()
                .map_err(|err| EnvStoreError::Io(err.to_string()))?;
            *store = next;
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            entries
                .iter()
                .position(|handle| handle.env.id == id)
                .map(|index| entries.remove(index))
        };
        self.publish();
        if let Some(handle) = handle {
            teardown(&handle).await;
        }
        if purge {
            let root = self.data_dir.join("environments").join(id);
            if let Err(err) = std::fs::remove_dir_all(&root)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(env = %id, error = %err, "environment purge failed");
            }
        }
        Ok(())
    }

    /// Engine shutdown: stop every environment's transports without touching the
    /// store. Live environment connections hold clones of these handles, so
    /// unreferencing them alone would leave sockets and rows open.
    pub async fn shutdown_all(&self) {
        let handles: Vec<EnvHandle> =
            std::mem::take(&mut *self.entries.write().unwrap_or_else(|e| e.into_inner()));
        for handle in handles {
            teardown(&handle).await;
        }
    }

    fn open_runtime(&self, env: &Environment) -> Result<EnvHandle, EnvAddError> {
        let namespace = env
            .namespace
            .clone()
            .ok_or_else(|| EnvAddError::Runtime("environment has no namespace".into()))?;
        let edge = EdgeConfig::with_static_token(env.url.clone(), env.token.clone())
            .with_device(self.device_id.clone());
        let runtime = WorkspaceRuntime::open(
            &self.data_dir.join("environments").join(&env.id),
            &self.device_id,
            self.device_name.clone(),
            &namespace.org_id,
            &namespace.user_id,
            self.default_harness,
            Some(edge),
            // The laptop is not a device in a remote environment.
            false,
        )
        .map_err(|err| EnvAddError::Runtime(err.to_string()))?;
        let links = LinkCache::new(LinkCacheConfig::new(
            env.url.clone(),
            Arc::new(StaticToken(env.token.clone())),
        ));
        Ok(EnvHandle {
            env: env.clone(),
            runtime,
            links,
        })
    }

    fn publish(&self) {
        self.listing.send_replace(self.list());
    }
}

/// Revoke an environment's transports before draining: live environment
/// connections hold clones of the runtime and links, so unreferencing alone
/// leaves the sockets open. `flush` (not `shutdown`) persists the snapshot
/// without stamping this controller as a device in that environment.
async fn teardown(handle: &EnvHandle) {
    handle.links.disconnect_all();
    handle.runtime.workspace.disconnect_edge();
    handle.runtime.doc_host.shutdown_workers().await;
    handle.runtime.doc_host.flush_all();
    handle.runtime.workspace.flush();
}

fn redacted(env: &Environment) -> Environment {
    Environment {
        token: String::new(),
        ..env.clone()
    }
}

/// The part of `GET /env` this engine needs.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerDescriptor {
    v: u32,
    org_id: String,
    user_id: String,
}

/// Dial `{url}/env` with the bearer. Bounded by a 5 s timeout so a black-holed
/// VPS fails with a message instead of hanging the settings page.
async fn probe_server(url: &str, token: &str) -> Result<ServerDescriptor, EnvAddError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|err| EnvAddError::Unreachable(err.to_string()))?;
    let response = client
        .get(format!("{url}/env"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| EnvAddError::Unreachable(err.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(EnvAddError::Unauthorized);
    }
    if !status.is_success() {
        return Err(EnvAddError::Unreachable(format!(
            "server answered {status} for GET /env"
        )));
    }
    let body: ServerDescriptor = response
        .json()
        .await
        .map_err(|err| EnvAddError::Unreachable(format!("GET /env: {err}")))?;
    if body.v != 1 {
        return Err(EnvAddError::VersionMismatch {
            server: body.v,
            client: 1,
        });
    }
    Ok(body)
}
