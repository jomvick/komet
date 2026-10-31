//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring komet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::comments::ReviewComment;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use komet_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use komet_engine::{
    Engine, EngineConfig, EngineCore, EngineRuntime, InstanceLock, environments::LOCAL_ENV_ID,
    rpc::AuthRpc,
};
use komet_proto::{
    AuthState, Chat, ChatIndicator, Device, EngineInfo, Environment, HarnessId, Session, Space,
    WorkspaceScope,
};
use komet_rpc::{
    RpcClient, RpcError, RpcReply, RpcService, ServiceRouter, connect_ws, connect_ws_for,
    memory_client, methods,
};

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.komet`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// WorkOS client id for production authentication.
    pub sync_token: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
    /// Start a detached engine child if no daemon is already serving IPC.
    /// Disabled in tests and for callers that explicitly want an embedded engine.
    pub managed_engine: bool,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// Which connection a watch pump reads, and what a lost connection means for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchOwner {
    /// This app's engine: a closed connection is a daemon that died, so the whole
    /// viewport re-probes and re-embeds.
    Engine,
    /// A paired device's environment: a closed connection is that device going
    /// away. The pump retries and the switcher shows the truth — it never touches
    /// the local workspace.
    Workspace(String),
}

/// Reachability of one workspace, as the switcher dot reads it. Driven by the
/// device stream: first frame online, a lost connection offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceStatus {
    Connecting,
    Online,
    Offline,
}

/// What the Context popover needs from the thread to agree with the timeline:
/// the compaction boundaries the doc actually carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadCompactions {
    /// Settled boundaries (completed, failed, cancelled).
    pub count: usize,
    /// A cycle is mid-flight — the thread says so even when the agent's usage
    /// stats never mention compaction at all.
    pub in_progress: bool,
    /// Why the newest boundary happened, as the agent worded it (`None` = the
    /// agent never said; the UI shows no reason rather than a guess).
    pub last_trigger: Option<String>,
}

/// One row of the sidebar switcher: the local machine plus every paired device.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceEntry {
    pub id: String,
    pub label: String,
    /// Host of that device's sync server; empty for the local machine.
    pub host: String,
    pub status: WorkspaceStatus,
    /// Devices registered in that workspace (its own machine, plus any other
    /// machine sharing that sync server).
    pub device_count: usize,
    /// Engine version of the machine that workspace runs on, as its own registry
    /// row reports it — `None` until that row has been seen.
    pub version: Option<String>,
}

/// Where the user was inside a paired device, restored when they come back.
#[derive(Debug, Clone, Default)]
struct WorkspaceMemory {
    selected_space: Option<String>,
    selected_device: Option<String>,
    selected_chat: Option<String>,
    no_project: bool,
}

/// Host part of a sync server URL, for the switcher's sub-label.
pub fn url_host(url: &str) -> String {
    let Some(rest) = url.split_once("://").map(|(_, rest)| rest) else {
        return String::new();
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.rsplit('@').next().unwrap_or(host).to_string()
}

/// The machine a paired workspace runs on: its own registry row, never the
/// controller's — this app is not a device inside that environment.
fn workspace_home_device(devices: &[Device], home: Option<&str>) -> Option<String> {
    devices
        .iter()
        .find(|device| Some(device.id.as_str()) != home)
        .or_else(|| devices.first())
        .map(|device| device.id.clone())
}

/// The engine version of the machine a workspace runs on, from its own registry
/// row — `None` while no row has landed.
fn workspace_version(devices: &[Device], own_id: Option<&str>) -> Option<String> {
    own_id
        .and_then(|id| devices.iter().find(|device| device.id == id))
        .or_else(|| devices.first())
        .and_then(|device| device.version.clone())
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &Arc<RpcClient>;
    fn mode(&self) -> EngineMode;
    /// A connection scoped to one environment (`?env=`), for the paired-device
    /// workspace. The local workspace keeps using [`EngineBackend::client`].
    async fn env_client(&self, env_id: &str) -> Result<Arc<RpcClient>, String>;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    /// The assembled core, published by the boot task. Environment connections
    /// are built from it, so an unset cell means "still assembling".
    core: Arc<tokio::sync::OnceCell<Arc<EngineCore>>>,
    boot_task: tokio::task::JoinHandle<()>,
    refresh_task: tokio::task::JoinHandle<()>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: Arc<RpcClient>,
    /// The runtime this engine was built on. Environment connections are made
    /// from gpui's main thread, which has no reactor: the transport tasks have
    /// to land here, not on the thread that happened to ask for a device.
    runtime_handle: tokio::runtime::Handle,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &Arc<RpcClient> {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn env_client(&self, env_id: &str) -> Result<Arc<RpcClient>, String> {
        let Some(core) = self.core.get() else {
            return Err("engine is still assembling; no environments yet".into());
        };
        let service = core.env_service(Some(env_id))?;
        let _entered = self.runtime_handle.enter();
        Ok(Arc::new(memory_client(service)))
    }
    async fn shutdown(&self) {
        self.boot_task.abort();
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
        self.refresh_task.abort();
    }
}

#[derive(Clone)]
enum DeferredEngineState {
    Waiting,
    Ready,
    Failed(String),
}

/// Serves engine identity and AuthRpc immediately, then holds data calls only
/// while a captured synced profile still needs organization onboarding.
/// Existing subscriptions attach to the assembled service without reconnecting.
struct DeferredEngineRpc {
    auth: AuthRpc,
    engine_info: EngineInfo,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
    service: Arc<tokio::sync::OnceCell<Arc<dyn RpcService>>>,
}

#[async_trait]
impl RpcService for DeferredEngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == methods::ENGINE_INFO {
            return RpcReply::value(&self.engine_info);
        }
        if method == methods::ENGINE_READY {
            let mut state = self.state.clone();
            return match wait_for_deferred_engine(&mut state).await {
                Ok(()) => RpcReply::value(&serde_json::json!({ "ready": true })),
                Err(message) => Err(RpcError::Failed(message)),
            };
        }
        if AuthRpc::handles(method) {
            return self.auth.handle(method, params).await;
        }

        let mut state = self.state.clone();
        loop {
            let current = { state.borrow().clone() };
            match current {
                DeferredEngineState::Waiting => {}
                DeferredEngineState::Ready => {
                    let service = self.service.get().ok_or_else(|| {
                        RpcError::Failed(
                            "embedded engine became ready without an RPC service".into(),
                        )
                    })?;
                    return service.handle(method, params).await;
                }
                DeferredEngineState::Failed(message) => return Err(RpcError::Failed(message)),
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

async fn wait_for_deferred_engine(
    state: &mut tokio::sync::watch::Receiver<DeferredEngineState>,
) -> Result<(), String> {
    loop {
        let current = { state.borrow().clone() };
        match current {
            DeferredEngineState::Waiting => {}
            DeferredEngineState::Ready => return Ok(()),
            DeferredEngineState::Failed(message) => return Err(message),
        }
        state
            .changed()
            .await
            .map_err(|_| "embedded engine assembly ended without a result".to_string())?;
    }
}

/// Routes the embedded engine's IPC connections per `?env=`. The local workspace
/// keeps answering from the deferred service — a viewport that connects during
/// cloud onboarding still gets EngineInfo and auth immediately — while an
/// environment connection waits for assembly and gets that environment's service.
struct DeferredEngineRouter {
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
    deferred: Arc<dyn RpcService>,
    core: Arc<tokio::sync::OnceCell<Arc<EngineCore>>>,
}

#[async_trait]
impl ServiceRouter for DeferredEngineRouter {
    async fn service_for(&self, env: Option<String>) -> Result<Arc<dyn RpcService>, String> {
        let Some(id) = env.filter(|id| *id != LOCAL_ENV_ID) else {
            return Ok(self.deferred.clone());
        };
        let mut state = self.state.clone();
        wait_for_deferred_engine(&mut state).await?;
        let core = self
            .core
            .get()
            .ok_or("embedded engine assembly ended without a core")?;
        core.env_service(Some(&id))
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: Arc<RpcClient>,
    url: String,
    /// IPC bearer, kept so environment connections can dial `?env=` on the same
    /// daemon the viewport attached to.
    token: Arc<str>,
    lifecycle_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// See [`InProcessEngine::runtime_handle`] — dialing a device needs the
    /// reactor, so the handshake runs there and only its result crosses back.
    runtime_handle: tokio::runtime::Handle,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &Arc<RpcClient> {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn env_client(&self, env_id: &str) -> Result<Arc<RpcClient>, String> {
        let url = self.url.clone();
        let token = self.token.clone();
        let env_id = env_id.to_string();
        let dialed = self
            .runtime_handle
            .spawn(async move { connect_ws_for(&url, &token, Some(&env_id)).await });
        match dialed.await {
            Ok(Ok(client)) => Ok(Arc::new(client)),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("environment dial was cancelled".into()),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; only stop our readiness probe.
        if let Some(task) = self.lifecycle_task.lock().await.take() {
            task.abort();
        }
    }
}

/// One workspace of the local engine, reached through its own `?env=` connection.
/// It borrows the engine's transport and lifecycle: dropping this handle drops
/// the connection, never the process.
struct WorkspaceEngine {
    client: Arc<RpcClient>,
    /// The engine this workspace rides — `InProcess` or `Remote` alike.
    mode: EngineMode,
}

#[async_trait]
impl EngineBackend for WorkspaceEngine {
    fn client(&self) -> &Arc<RpcClient> {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        self.mode.clone()
    }
    async fn env_client(&self, _env_id: &str) -> Result<Arc<RpcClient>, String> {
        Err("already inside a device workspace".into())
    }
    async fn shutdown(&self) {
        // The pumps hold their own client clones; the connection closes once the
        // last one is dropped. There is nothing here to drain.
    }
}

/// Whether anything accepts a TCP connection on the IPC port. A refused
/// connection (or a probe timeout) means the port is free for an engine to
/// claim.
async fn port_is_listening(ipc_port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
    engine_info: EngineInfo,
    deferred_state: Option<tokio::sync::watch::Receiver<DeferredEngineState>>,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        // Invariant: at most one bootstrap in this process runs probe+embed at
        // a time. The winner binds the deferred IPC listener before releasing
        // the gate, so a concurrent viewport's probe finds it and attaches as
        // Remote instead of racing it for the data dir.
        static BOOTSTRAP_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _gate = BOOTSTRAP_GATE.lock().await;

        if let Some(handle) = Self::attach_to_daemon(config.ipc_port).await {
            return Ok(handle);
        }

        if config.managed_engine
            && let Some(handle) = Self::start_managed_engine(&config).await
        {
            return Ok(handle);
        }

        // Reached when nothing attached: no daemon, a foreign listener, a
        // managed engine that failed or timed out, or managed engines are off.
        // The reason was already logged by whichever probe failed.
        tracing::info!(
            data_dir = %config.data_dir.display(),
            port = config.ipc_port,
            "embedding engine"
        );
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            org_id: config.org_id,
            sync_token: config.sync_token,
        };

        // Own the data dir before opening anything under it or binding IPC —
        // the lock, not the port bind, is the ownership decision. A failed
        // acquire means an out-of-process engine holds the dir but was not
        // serving IPC at probe time (a daemon mid-start): wait for its
        // listener, re-trying the lock in case it dies instead.
        std::fs::create_dir_all(&engine_config.data_dir)?;
        let lock_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let lock = loop {
            match InstanceLock::acquire(&engine_config.data_dir) {
                Ok(lock) => break lock,
                Err(err) => {
                    if std::time::Instant::now() >= lock_deadline {
                        return Err(err.into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if let Some(handle) = Self::attach_to_daemon(engine_config.ipc_port).await {
                        return Ok(handle);
                    }
                }
            }
        };

        let auth = Engine::build_auth(&engine_config).await;
        let workspace_scope = Engine::initial_workspace_scope(&auth);
        let initial_profile = Engine::resolve_profile(&engine_config, &auth, workspace_scope)?;
        let profile_is_resolved = initial_profile.is_some();
        let engine_info = Engine::engine_info(&engine_config, workspace_scope)?;
        let refresh_task = auth.spawn_refresh_loop();
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let assembled_service = Arc::new(tokio::sync::OnceCell::new());
        let assembled_core: Arc<tokio::sync::OnceCell<Arc<EngineCore>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let service: Arc<dyn RpcService> = Arc::new(DeferredEngineRpc {
            auth: AuthRpc::new(auth.clone()),
            engine_info: engine_info.clone(),
            state: state_rx.clone(),
            service: assembled_service.clone(),
        });
        let client = Arc::new(memory_client(service.clone()));

        // Serve the same service on the IPC port so a terminal viewport can
        // attach to this window's engine with no setup. Deliberately the
        // *deferred* service, not the assembled one: a viewport that connects
        // during cloud onboarding gets EngineInfo and AuthRpc immediately, and
        // its data subscriptions wait exactly as this window's do.
        //
        // Best-effort — losing the bind race with another engine costs other
        // viewports, not this one.
        let ipc_task = match komet_engine::serve_ipc_with_router(
            engine_config.ipc_port,
            Arc::new(DeferredEngineRouter {
                state: state_rx.clone(),
                deferred: service,
                core: assembled_core.clone(),
            }),
        )
        .await
        {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(None));
        let runtime_for_boot = runtime.clone();
        let service_for_boot = assembled_service.clone();
        let core_for_boot = assembled_core.clone();
        // The instance lock rides into the boot task and is consumed by
        // assembly — held through sign-in onboarding too, because this process
        // owns the data dir from the moment it decided to embed.
        let boot_task = tokio::spawn(async move {
            let profile = match initial_profile {
                Some(profile) => profile,
                None => {
                    let mut auth_state = auth.watch_state();
                    while !auth_state.borrow().is_signed_in() {
                        if auth_state.changed().await.is_err() {
                            state_tx.send_replace(DeferredEngineState::Failed(
                                "authentication state closed before workspace onboarding".into(),
                            ));
                            return;
                        }
                    }
                    match Engine::resolve_profile(&engine_config, &auth, workspace_scope) {
                        Ok(Some(profile)) => profile,
                        Ok(None) => {
                            state_tx.send_replace(DeferredEngineState::Failed(
                                "workspace onboarding completed without an organization".into(),
                            ));
                            return;
                        }
                        Err(err) => {
                            state_tx.send_replace(DeferredEngineState::Failed(err.to_string()));
                            return;
                        }
                    }
                }
            };

            match Engine::assemble_runtime_with_lock(&engine_config, auth, profile, lock).await {
                Ok(engine_runtime) => {
                    let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
                    // Published before Ready: an environment connection that sees
                    // Ready must find a core.
                    let _ = core_for_boot.set(engine_runtime.core_arc());
                    *runtime_for_boot.lock().await = Some(engine_runtime);
                    if service_for_boot.set(service).is_err() {
                        state_tx.send_replace(DeferredEngineState::Failed(
                            "embedded engine RPC service was assembled more than once".into(),
                        ));
                        return;
                    }
                    state_tx.send_replace(DeferredEngineState::Ready);
                }
                Err(err) => {
                    tracing::error!(error = %err, "embedded engine assembly failed");
                    state_tx.send_replace(DeferredEngineState::Failed(format!("{err:#}")));
                }
            }
        });
        let handle = EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                core: assembled_core,
                boot_task,
                refresh_task,
                ipc_task,
                client,
                runtime_handle: tokio::runtime::Handle::current(),
            }),
            engine_info,
            deferred_state: Some(state_rx.clone()),
        };
        // Local, development, and already-resolved synced profiles need no
        // authentication UI while assembling. Keep the viewport Connecting
        // until their stores and journals are actually open, and surface a
        // boot failure through the existing bootstrap error path.
        if profile_is_resolved && let Err(message) = wait_for_deferred_engine(&mut state_rx).await {
            handle.shutdown().await;
            return Err(anyhow::anyhow!(message));
        }
        Ok(handle)
    }

    /// Launch this app's engine as a detached sibling process and attach over
    /// the same authenticated IPC used by explicitly installed daemons.
    async fn start_managed_engine(config: &EngineBootConfig) -> Option<EngineHandle> {
        // A listener that did not attach above is not ours (another app on the
        // same port, or an engine of another user). Spawning a managed engine
        // is then guaranteed to lose the bind, and probing it would repeat the
        // token warning every 150ms — embed once instead.
        if port_is_listening(config.ipc_port).await {
            tracing::warn!(
                port = config.ipc_port,
                "IPC port is held by a listener that is not this komet's engine; \
                 embedding this window instead of starting a managed engine"
            );
            return None;
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(err) => {
                tracing::warn!(error = %err, "could not locate app executable for managed engine");
                return None;
            }
        };
        let mut command = std::process::Command::new(executable);
        command
            .arg("engine")
            .env("KOMET_DATA_DIR", &config.data_dir)
            .env("KOMET_IPC_PORT", config.ipc_port.to_string())
            .env("KOMET_EDGE_URL", &config.edge_url)
            .env("KOMET_HARNESS", config.default_harness.as_str())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        // The engine logs through its own file; stderr only carries panics and
        // failures before logging is up. Discarding it made a dead managed
        // engine undiagnosable, so append it next to the other logs.
        let child_log = config.data_dir.join("logs").join("komet-engine-child.log");
        let stderr = std::fs::create_dir_all(config.data_dir.join("logs"))
            .ok()
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&child_log)
                    .ok()
            })
            .map(std::process::Stdio::from)
            .unwrap_or_else(std::process::Stdio::null);
        command.stderr(stderr);
        match &config.edge_token {
            Some(token) => {
                command.env("KOMET_EDGE_TOKEN", token);
            }
            None => {
                command.env_remove("KOMET_EDGE_TOKEN");
            }
        }
        match &config.sync_token {
            Some(token) => {
                command.env("KOMET_SYNC_TOKEN", token);
            }
            None => {
                command.env_remove("KOMET_SYNC_TOKEN");
            }
        }
        match &config.org_id {
            Some(org_id) => {
                command.env("KOMET_ORG_ID", org_id);
            }
            None => {
                command.env_remove("KOMET_ORG_ID");
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED: u32 = 0x0000_0208 | 0x0800_0000;
            command.creation_flags(DETACHED | 0x0100_0000);
        }

        #[cfg(windows)]
        let spawned = {
            use std::os::windows::process::CommandExt as _;
            command.spawn().or_else(|_| {
                command.creation_flags(0x0000_0208 | 0x0800_0000);
                command.spawn()
            })
        };
        #[cfg(not(windows))]
        let spawned = command.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(error = %err, "could not start managed engine; embedding instead");
                return None;
            }
        };
        tracing::info!(pid = child.id(), "started detached managed engine");

        // The engine assembles stores and live model catalogs before it binds
        // IPC: 37s measured warm, longer when the machine is loaded (a 7-minute
        // build or shader compile leaves the disk cold). A 60s cap killed
        // healthy children and forced the slower embedded path. Wait for the
        // real condition instead — IPC attach or child exit — with a generous
        // cap and a heartbeat so a slow boot is visible in the log.
        const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
        const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let deadline = started + START_TIMEOUT;
        let mut next_progress = started + PROGRESS_EVERY;
        loop {
            if let Some(handle) = Self::attach_to_daemon(config.ipc_port).await {
                reap_child(child);
                return Some(handle);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!(%status, "managed engine exited before IPC became ready; embedding instead");
                    return None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "could not monitor managed engine; embedding instead");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => {}
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    waited_s = started.elapsed().as_secs(),
                    "managed engine did not become ready; killing it and embedding instead"
                );
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            if now >= next_progress {
                tracing::info!(
                    elapsed_s = started.elapsed().as_secs(),
                    "managed engine still starting (it binds IPC after assembling stores and model catalogs)"
                );
                next_progress += PROGRESS_EVERY;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }

    /// Probe the IPC port and, if a live engine answers, attach as a remote
    /// viewport. `None` means embed: nothing listening, a non-engine listener,
    /// or a listener without an identity.
    async fn attach_to_daemon(ipc_port: u16) -> Option<EngineHandle> {
        let url = format!("ws://127.0.0.1:{ipc_port}");
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
        )
        .await;
        if !matches!(probe, Ok(Ok(_))) {
            return None;
        }

        // Read the identity BEFORE claiming a daemon: the port may be held by
        // a different app. This runs every 150ms while a managed engine boots,
        // so the diagnostic stays at debug level and the wait loop above
        // reports the foreign-listener case exactly once.
        let token = match komet_rpc::ipc_token::read_default(ipc_port) {
            Ok(token) => token,
            Err(err) => {
                tracing::debug!(
                    port = ipc_port,
                    error = %err,
                    "port is in use but is not this komet's engine"
                );
                return None;
            }
        };
        tracing::info!(%url, "engine daemon detected; connecting");
        match connect_ws(&url, &token).await {
            Ok(client) => match query_engine_info(&client).await {
                Ok(engine_info) => {
                    let client = Arc::new(client);
                    let (state_tx, state_rx) =
                        tokio::sync::watch::channel(DeferredEngineState::Waiting);
                    let lifecycle_client = client.clone();
                    let lifecycle_task = tokio::spawn(async move {
                        let state = match lifecycle_client
                            .call(methods::ENGINE_READY, serde_json::json!({}))
                            .await
                        {
                            Ok(_) => DeferredEngineState::Ready,
                            // EngineReady was added after EngineInfo. An older daemon
                            // that does not expose the barrier is already assembled.
                            Err(RpcError::Failed(message))
                                if message
                                    == format!("unknown method: {}", methods::ENGINE_READY) =>
                            {
                                DeferredEngineState::Ready
                            }
                            Err(err) => DeferredEngineState::Failed(err.to_string()),
                        };
                        state_tx.send_replace(state);
                    });
                    Some(EngineHandle {
                        inner: Arc::new(RemoteEngine {
                            client,
                            url,
                            token: token.as_str().into(),
                            lifecycle_task: tokio::sync::Mutex::new(Some(lifecycle_task)),
                            runtime_handle: tokio::runtime::Handle::current(),
                        }),
                        engine_info,
                        deferred_state: Some(state_rx),
                    })
                }
                Err(err) => {
                    tracing::warn!(
                        %url,
                        error = %err,
                        "listener did not provide engine identity; embedding instead"
                    );
                    None
                }
            },
            // Something is on the port but it is not an engine (or it is
            // wedged). Fall through and embed: a stranger holding 27654
            // should cost other viewports, not this window.
            Err(err) => {
                tracing::warn!(%url, error = %err, "not an engine; embedding instead");
                None
            }
        }
    }

    pub fn client(&self) -> &Arc<RpcClient> {
        self.inner.client()
    }

    /// Shared handle for a watch pump that must outlive the `EngineHandle` borrow.
    pub fn client_arc(&self) -> Arc<RpcClient> {
        self.inner.client().clone()
    }

    /// One connection scoped to a paired device's environment. Callers keep the
    /// handle: a connection per environment, not per call.
    pub async fn env_client(&self, env_id: &str) -> Result<Arc<RpcClient>, String> {
        if let Some(mut state) = self.deferred_state() {
            wait_for_deferred_engine(&mut state).await?;
        }
        self.inner.env_client(env_id).await
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    /// Wrap one environment connection so the shell's existing `state.engine()`
    /// call sites speak to the workspace being displayed.
    fn for_workspace(client: Arc<RpcClient>, engine_info: EngineInfo, mode: EngineMode) -> Self {
        EngineHandle {
            inner: Arc::new(WorkspaceEngine { client, mode }),
            engine_info,
            deferred_state: None,
        }
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    fn deferred_state(&self) -> Option<tokio::sync::watch::Receiver<DeferredEngineState>> {
        self.deferred_state.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

fn reap_child(mut child: std::process::Child) {
    if let Err(err) = std::thread::Builder::new()
        .name("komet-engine-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        tracing::warn!(error = %err, "could not create managed-engine reaper thread");
    }
}

/// Query the current protocol first, with a conservative fallback for daemons
/// from before `EngineInfo` existed. Old daemons are always treated as synced.
async fn query_engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
    match client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
    {
        Ok(info) => Ok(info),
        Err(RpcError::Failed(message))
            if message == format!("unknown method: {}", methods::ENGINE_INFO) =>
        {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct LocalDevice {
                device_id: String,
            }
            let legacy: LocalDevice = client
                .call_as(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await?;
            Ok(EngineInfo {
                device_id: legacy.device_id,
                workspace_scope: WorkspaceScope::Synced,
            })
        }
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `komet_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use komet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, RECONNECT_MAX_ATTEMPTS, SESSION_STALE_MS,
    attention_rank, chat_location, display_status, effective_indicator, format_ms, format_time_ago,
    gate_phase, group_chats, next_reconnect_step, parse_auth_state, project_label,
    reconnect_base_delay_ms, reconnect_jittered_delay_ms, reconnect_should_retry, sort_active,
    sort_chats, sort_spaces, sort_tabs,
};

// ---------------------------------------------------------------------------
// Org gate (pure)
// ---------------------------------------------------------------------------

/// One org membership row (tolerant local mirror of the engine's ListOrgs
/// reply — `{orgs: [{id, organizationId, name}]}`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRow {
    pub organization_id: String,
    pub name: String,
}

/// Parse a ListOrgs reply tolerantly (accepts a bare array too).
pub fn parse_orgs(value: &serde_json::Value) -> Vec<OrgRow> {
    let list = value.get("orgs").unwrap_or(value);
    serde_json::from_value(list.clone()).unwrap_or_default()
}

/// Workspace names must be non-empty (trimmed) and reasonably short.
pub fn org_name_valid(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 64
}

/// Memberships sorted by name (case-insensitive), deduped by organization id.
pub fn sort_memberships(mut orgs: Vec<OrgRow>) -> Vec<OrgRow> {
    orgs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    orgs.dedup_by(|a, b| a.organization_id == b.organization_id);
    orgs
}

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// A composer send whose doc command is queued but not yet executed by the
/// chat's host device — cleared when the host writes the user message back
/// into the transcript (same client-minted id as the [`AppState::echoes`]
/// dedup), or after [`PENDING_SEND_TTL_MS`].
#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started: DateTime<Utc>,
}

/// How long the send-in-flight overlay may hold before the synced status
/// shows through again. Covers the queue → nudge → drain → sync round-trip
/// to a remote host; when the host is offline the dot falls back to the
/// truth after this.
pub const PENDING_SEND_TTL_MS: i64 = 30_000;

struct UploadProgress {
    done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    total: u64,
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Fixed data boundary of the attached engine. Authentication may change
    /// in place, but changing this scope requires assembling a new runtime.
    pub workspace_scope: Option<WorkspaceScope>,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// The project the new-session canvas mints into. Healed by
    /// [`Self::apply_spaces`] when the row vanishes; selecting a chat implies
    /// its project.
    pub selected_space: Option<String>,
    /// Deliberate "Don't work in a project" pick: while set, the canvas mints
    /// project-less sessions (cwd `~` on the picked device) and
    /// [`Self::selected_space_row`] reads as `None` — healing must NOT
    /// re-select a project underneath it.
    pub no_project: bool,
    /// The composer's device pick — where project-less sessions run, and the
    /// device whose projects the project picker lists. `None` falls back to
    /// the local device.
    pub selected_device: Option<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// First chats / spaces watch frame has landed — device-local state that
    /// prunes against the doc (open tabs, the sidebar space filter) must not
    /// judge by the empty pre-sync lists.
    pub chats_synced: bool,
    pub spaces_synced: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// The selected chat's current plan: latest `ToolCall::Todo` in doc order,
    /// cached so the persistent plan card never rescans a long transcript on
    /// every streaming frame.
    pub active_plan: Option<Vec<komet_proto::TodoItem>>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// Send-in-flight overlay per chat id: a queued doc command the host
    /// hasn't executed yet (see [`Self::begin_pending_send`]).
    pending_sends: HashMap<String, PendingSend>,
    /// The in-flight send's attachment upload, when it has one.
    upload_progress: Option<UploadProgress>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully). Follows the active
    /// workspace: in a paired device it is that device, so every
    /// `targetDeviceId` decision reads correctly.
    pub local_device_id: Option<String>,
    /// The machine this app's own engine runs on — never changes, and says which
    /// registry row is "me" in the local workspace.
    pub home_device_id: Option<String>,
    /// Active workspace: [`LOCAL_ENV_ID`] or a paired device's environment id.
    pub active_workspace: String,
    /// The switcher's paired-device rows (the local machine is implied, first).
    pub workspaces: Vec<WorkspaceEntry>,
    /// Raw registry rows of the paired devices (the local row excluded). Kept for
    /// Settings → Sync: the host, the added-at, and the link re-encode.
    pub paired_envs: Vec<Environment>,
    /// First `WatchEnvironments` frame has landed — an empty list is only
    /// "nothing paired" once this is true.
    pub env_list_synced: bool,
    /// Device rows of the local workspace, kept while a paired device is
    /// displayed so the switcher's local row keeps its real hostname.
    local_devices: Vec<Device>,
    /// Connection of the workspace being displayed; `None` while a paired device
    /// is being dialed — the local engine is never a fallback for a remote
    /// workspace, or its rows would land under the wrong label.
    display_engine: Option<EngineHandle>,
    /// One multiplexed connection per paired device, alive while it is paired.
    env_clients: HashMap<String, Arc<RpcClient>>,
    /// Watches of the active paired device (empty while the local workspace is).
    env_watches: Vec<Task<()>>,
    /// A standing device stream per paired device: it carries the switcher's
    /// online dot, the workspace's device rows, and its connection.
    env_presence: HashMap<String, Task<()>>,
    /// Device rows of the paired devices, kept warm for status and switching.
    env_devices: HashMap<String, Vec<Device>>,
    /// Per-device navigation memory (space/device/chat selection).
    env_memory: HashMap<String, WorkspaceMemory>,
    /// The `WatchEnvironments` stream of the local engine.
    workspaces_task: Option<Task<()>>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<komet_update::UpdateStatus>,
    /// Cumulative context & token usage metrics per chat id.
    pub context_usage: HashMap<String, komet_proto::ContextUsageStats>,
    /// Written by the changes/files pane, read by the composer.
    review_comments: HashMap<String, Vec<ReviewComment>>,
    /// File surfaces whose editor-backed comments currently cite a buffer
    /// revision that has not reached disk yet. A chat remains blocked until
    /// every surface waiting on a workspace write has finished or cancelled.
    review_comment_flushes: HashMap<String, HashSet<u64>>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`,
    /// `security-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    pub workspace_state: Option<komet_proto::WorkspaceState>,
    /// The sandbox level the next message is sent with. It follows the open
    /// chat (see [`access_for_selection`]) and is updated by
    /// [`AppState::sync_access_mode`]; it is not the default for new chats.
    pub access_mode: komet_proto::SandboxLevel,
    /// The level new chats start at, from the Security settings.
    pub new_chat_access: komet_proto::SandboxLevel,
    /// Access-chip click for the current selection (`None` = new-chat canvas).
    /// Held until the persisted row (or Security default) catches up, so a
    /// chats watch frame cannot snap Full access back to Sandboxed.
    pending_access: Option<(Option<String>, komet_proto::SandboxLevel)>,
    /// In-flight `setChatConfig` echo. A chats watch frame that left before
    /// the mutate landed would otherwise replace the stamped row and snap
    /// the access chip back to Sandboxed.
    pending_chat_config: Option<(String, komet_proto::ChatConfig)>,
    engine: Option<EngineHandle>,
    boot_config: Option<EngineBootConfig>,
    /// Watches that follow the engine, not the displayed workspace: assembly
    /// result, auth, updates, this machine's device id.
    watch_tasks: Vec<Task<()>>,
    /// The local workspace's row watches — stopped while a paired device is
    /// displayed, so its frames can't repaint another workspace's lists.
    data_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
    /// Ordinal of the auto-retry attempt in flight, 0 when the chain is idle
    /// (see [`Self::schedule_reconnect`] and the bounded schedule in
    /// `komet_proto::view`).
    pub reconnect_attempt: u32,
    /// Wall clock the pending retry is due at — what the banner counts down
    /// against. `None` whenever no attempt is queued.
    pub reconnect_deadline: Option<DateTime<Utc>>,
    /// A chain attempt is queued or dialing right now. This, and not the status
    /// label, is what keeps two dying watches from forking two retry chains —
    /// the status stays `Reconnecting` across a failure so the countdown never
    /// jumps.
    reconnect_pending: bool,
    /// The sleeping retry attempt. Kept so a manual Retry (which restarts
    /// `bootstrap`) can cancel it: dropping a gpui task cancels it, whereas a
    /// detached one wakes later and forks a second chain.
    reconnect_task: Option<Task<()>>,
    /// Messages the user fired while the link was down: kept whole here, named
    /// by the reason the UI showed, and tagged with the chat they were typed
    /// in so a later drain cannot deliver them to another chat. Handed back
    /// once by [`Self::take_queued_sends_for`]. A cut-off send must not be a
    /// lost paragraph.
    queued_sends: Vec<QueuedSend>,
}

/// One parked message (transparency plan Task 10). The reason is stored with it
/// because that is what makes the wait legible: "queued — reconnecting (2)"
/// reads as a deliberate hold, a silent input box reads as a dead button. The
/// chat key is the owner: `""` is the new-chat canvas, every other key is the
/// chat that was selected when the send was held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSend {
    pub chat_key: String,
    pub text: String,
    pub reason: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            workspace_scope: None,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_space: None,
            no_project: false,
            selected_device: None,
            selected_chat: None,
            transcript: Vec::new(),
            active_plan: None,
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            upload_progress: None,
            local_device_id: None,
            home_device_id: None,
            active_workspace: LOCAL_ENV_ID.to_string(),
            workspaces: Vec::new(),
            paired_envs: Vec::new(),
            env_list_synced: false,
            local_devices: Vec::new(),
            display_engine: None,
            env_clients: HashMap::new(),
            env_watches: Vec::new(),
            env_presence: HashMap::new(),
            env_devices: HashMap::new(),
            env_memory: HashMap::new(),
            workspaces_task: None,
            update: None,
            context_usage: HashMap::new(),
            review_comments: HashMap::new(),
            review_comment_flushes: HashMap::new(),
            data_dir: None,
            workspace_state: None,
            access_mode: komet_proto::SandboxLevel::WorkspaceWrite,
            new_chat_access: komet_proto::SandboxLevel::WorkspaceWrite,
            pending_access: None,
            pending_chat_config: None,
            engine: None,
            boot_config: None,
            watch_tasks: Vec::new(),
            data_tasks: Vec::new(),
            transcript_task: None,
            reconnect_attempt: 0,
            reconnect_deadline: None,
            reconnect_pending: false,
            reconnect_task: None,
            queued_sends: Vec::new(),
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
        }
    }

    pub fn review_comments(&self, key: &str) -> &[ReviewComment] {
        self.review_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, key: &str, comment: ReviewComment) {
        self.review_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_review_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.review_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.review_comments.remove(key);
                self.review_comment_flushes.remove(key);
            }
        }
    }

    pub fn update_review_comment_line(&mut self, key: &str, id: &str, line: u32) {
        if let Some(comment) = self
            .review_comments
            .get_mut(key)
            .and_then(|comments| comments.iter_mut().find(|comment| comment.id == id))
        {
            comment.line = line;
        }
    }

    pub fn rename_review_comment_path(&mut self, key: &str, old_path: &str, new_path: &str) {
        if let Some(comments) = self.review_comments.get_mut(key) {
            for comment in comments
                .iter_mut()
                .filter(|comment| comment.is_file() && comment.path == old_path)
            {
                comment.path = new_path.to_string();
            }
        }
    }

    pub fn take_review_comments(&mut self, key: &str) -> Vec<ReviewComment> {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_review_comments(&mut self, key: &str) {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key);
    }

    pub fn begin_review_comment_flush(&mut self, key: &str, source: u64) {
        self.review_comment_flushes
            .entry(key.to_string())
            .or_default()
            .insert(source);
    }

    pub fn finish_review_comment_flush(&mut self, key: &str, source: u64) {
        let remove_key = self
            .review_comment_flushes
            .get_mut(key)
            .is_some_and(|sources| {
                sources.remove(&source);
                sources.is_empty()
            });
        if remove_key {
            self.review_comment_flushes.remove(key);
        }
    }

    pub fn review_comment_flush_pending(&self, key: &str) -> bool {
        self.review_comment_flushes.contains_key(key)
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        // Keep the optimistic setChatConfig stamp until the watch frame
        // carries the same config (or the chat disappears). Otherwise a
        // snapshot that left before the mutate landed snaps the access chip
        // from Full access back to Sandboxed.
        self.pending_chat_config = match self.pending_chat_config.take() {
            Some((id, pending)) => match chats.iter_mut().find(|c| c.id == id) {
                Some(chat) if chat.config.as_ref() == Some(&pending) => None,
                Some(chat) => {
                    chat.config = Some(pending.clone());
                    Some((id, pending))
                }
                None => None,
            },
            None => None,
        };
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.pending_access = None;
            self.transcript.clear();
            self.active_plan = None;
            self.transcript_task = None;
        }
        // The update may change the open chat's level from another device, or
        // remove the open chat, so recompute the level.
        self.sync_access_mode();
    }

    /// Recompute [`AppState::access_mode`] from the current selection.
    pub fn sync_access_mode(&mut self) {
        let resolved = access_for_selection(
            &self.chats,
            self.selected_chat.as_deref(),
            self.new_chat_access,
        );
        let pending = self.pending_access.take();
        self.access_mode = match pending {
            Some((owner, level)) if owner.as_deref() == self.selected_chat.as_deref() => {
                if resolved == level {
                    resolved
                } else {
                    // Click has not landed in the saved row yet (new-chat draft,
                    // unknown harness, or a stale chats frame).
                    self.pending_access = Some((owner, level));
                    level
                }
            }
            _ => resolved,
        };
    }

    /// Advance the access chip. Watch frames keep this level until the open
    /// chat's saved config (or the new-chat default) matches it.
    pub fn set_access_mode(&mut self, level: komet_proto::SandboxLevel) {
        self.pending_access = Some((self.selected_chat.clone(), level));
        self.access_mode = level;
    }

    /// Security-settings default. On the new-chat screen the chip follows
    /// the value the user just picked.
    pub fn set_new_chat_access(&mut self, level: komet_proto::SandboxLevel) {
        self.new_chat_access = level;
        if self.selected_chat.is_none() {
            self.pending_access = None;
        }
        self.sync_access_mode();
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        for session in &sessions {
            if let Some(stats) = &session.context_usage {
                self.context_usage
                    .insert(session.chat_id.clone(), stats.clone());
            }
        }
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        // Heal a vanished selection (project deleted elsewhere): fall back to
        // the first project; its chats died with it, so a matching chat
        // selection is healed by the accompanying chats frame (`apply_chats`).
        // The picker lists projects per-device, so healing prefers one on the
        // picked device — a global fallback would silently re-aim the canvas
        // at another machine.
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.first_space_on_picked_device();
        }
        // First frame with no selection yet: pick the first project so the
        // canvas never boots project-less by accident — unless the user
        // deliberately opted out.
        if self.selected_space.is_none() && !self.no_project {
            self.selected_space = self.first_space_on_picked_device();
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: komet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config.clone());
        }
        self.pending_chat_config = Some((chat_id.to_string(), config));
        self.sync_access_mode();
    }

    /// Drop an in-flight config echo (mutate failed) so the next watch frame
    /// is allowed to replace the stamped row.
    pub fn discard_pending_chat_config(&mut self) {
        self.pending_chat_config = None;
        self.sync_access_mode();
    }

    pub fn apply_devices(&mut self, mut devices: Vec<Device>) {
        if self.is_local_workspace() {
            self.local_devices = devices.clone();
        }
        // A local-only workspace has no remote device identity to distinguish.
        // Keep the engine's legacy sentinel out of the UI while preserving real
        // hostnames and user-assigned device names.
        if self.workspace_scope == Some(WorkspaceScope::Local)
            && let Some(local_id) = self.local_device_id.as_deref()
            && let Some(device) = devices.iter_mut().find(|device| device.id == local_id)
            && device.name == "unknown-device"
        {
            device.name = "Local".to_string();
        }
        self.devices = devices;
    }

    /// First project on the composer's picked device (falling back through
    /// the local device, then any project at all — better a cross-device
    /// project than a surprise project-less canvas). Display order.
    fn first_space_on_picked_device(&self) -> Option<String> {
        let device = self
            .selected_device
            .as_deref()
            .or(self.local_device_id.as_deref());
        let sorted = self.spaces_sorted();
        device
            .and_then(|d| sorted.iter().find(|s| s.device_id == d).copied())
            .or_else(|| sorted.first().copied())
            .map(|s| s.id.clone())
    }

    pub fn apply_update(&mut self, status: komet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_context_usage(&mut self, chat_id: &str, stats: komet_proto::ContextUsageStats) {
        self.context_usage.insert(chat_id.to_string(), stats);
    }

    /// Derives or returns the current context usage metrics for the selected chat.
    pub fn current_context_usage(&self) -> komet_proto::ContextUsageStats {
        let estimated = self.transcript_token_estimate();
        if let Some(chat_id) = &self.selected_chat
            && let Some(mut stats) = self.context_usage.get(chat_id).cloned()
        {
            // Cursor (and similar) bill tool-loop prompt tokens far past the
            // window, then the adapter used to clamp that to 100%. That is
            // not occupancy — the session keeps going. Prefer the transcript
            // estimate so the ring can move instead of pinning full.
            let billed_overshoot = stats.source == komet_proto::ContextUsageSource::Approximate
                && stats.context_limit > 0
                && stats.input_tokens > stats.context_limit
                && stats.used() >= stats.context_limit;
            if billed_overshoot && estimated > 0 {
                stats.used_tokens = estimated;
                stats.source = komet_proto::ContextUsageSource::Estimated;
            }
            return stats;
        }
        let model_name = self
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
            .and_then(|cfg| cfg.model.as_deref())
            .unwrap_or("claude-3-7-sonnet");
        let limit = komet_proto::default_context_limit_for_model(model_name);
        let mut stats = komet_proto::ContextUsageStats::new(limit);
        stats.source = komet_proto::ContextUsageSource::Estimated;
        stats.input_tokens = estimated;
        stats
    }

    fn transcript_token_estimate(&self) -> u64 {
        let mut total_chars: usize = 0;
        for entry in &self.transcript {
            for part in &entry.parts {
                match part {
                    komet_doc::MessagePart::Text { text, .. } => {
                        total_chars += text.len();
                    }
                    komet_doc::MessagePart::Tool {
                        output: Some(out), ..
                    } => {
                        total_chars += out.len();
                    }
                    _ => {}
                }
            }
        }
        (total_chars / 4) as u64
    }

    /// What the thread itself carries about compaction: the boundaries the doc
    /// holds, and why the newest one happened. Harnesses are inconsistent —
    /// some report a count in their usage stats, some report nothing — so the
    /// Context popover reads the doc too: a boundary in the transcript is a
    /// fact, while a missing stat is only silence.
    pub fn thread_compactions(&self) -> ThreadCompactions {
        let mut out = ThreadCompactions::default();
        for entry in &self.transcript {
            for part in &entry.parts {
                if let komet_doc::MessagePart::Compaction {
                    status, trigger, ..
                } = part
                {
                    match status {
                        komet_proto::CompactionStatus::InProgress => out.in_progress = true,
                        _ => out.count += 1,
                    }
                    // Newest wins: entries arrive in order, so the last
                    // settled boundary's trigger is the one the popover shows.
                    if let Some(trigger) = trigger {
                        out.last_trigger = Some(trigger.clone());
                    }
                }
            }
        }
        out
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&komet_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.refresh_active_plan();
        self.ack_pending_send_from_transcript();
    }

    /// Recompute [`Self::active_plan`] from the transcript. Pure backward scan
    /// (last `Todo` wins), so it is cheap while the plan sits near the tail.
    fn refresh_active_plan(&mut self) {
        self.active_plan = komet_doc::latest_plan(&self.transcript);
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        let had_entries = self.transcript.len();
        komet_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        // Plans update in place on the streaming tail; only that case (or a
        // new/removed entry) can change the answer. Skipping the scan keeps
        // frame-time cost flat for long chats.
        let tail_has_plan = self.transcript.last().is_some_and(|entry| {
            entry.parts.iter().any(|part| {
                matches!(
                    part,
                    komet_doc::MessagePart::Tool {
                        call: komet_proto::ToolCall::Todo { .. },
                        ..
                    }
                )
            })
        });
        if tail_has_plan || self.transcript.len() != had_entries {
            self.refresh_active_plan();
        }
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.ack_pending_send_from_transcript();
        Ok(())
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Truncate transcript from a target message ID and clear pending echoes.
    /// Used when editing a prompt to erase the prompt and all its subsequent responses.
    pub fn truncate_transcript_from(&mut self, chat_id: &str, target_message_id: &str) {
        if let Some(idx) = self
            .transcript
            .iter()
            .position(|e| e.id == target_message_id)
        {
            self.transcript.truncate(idx);
        }
        self.refresh_active_plan();
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.clear();
        }
    }

    /// Composer send fired: overlay the chat as Working until the host writes
    /// the user message back into the transcript (or the TTL lapses). A remote
    /// send has no live session row until the host drains the queued command —
    /// that gap read as "no live run" and flashed the Completed dot, and any
    /// phantom Working→Idle edge in it rang the done-chime on send (user
    /// report 2026-08-05).
    pub fn begin_pending_send(&mut self, chat_id: &str, message_id: &str, now: DateTime<Utc>) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started: now,
            },
        );
    }

    /// Send failed — drop the overlay so the dot tells the truth again. Only
    /// removes the overlay this message started: a quick resend must not lose
    /// its own overlay to the first send's failure cleanup.
    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|p| p.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Is a send still in flight for this chat (unacked, inside the TTL)?
    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
        })
    }

    /// When the in-flight send (if any, inside the TTL) was fired — the
    /// elapsed-timer base while the overlay reads as Working. The session
    /// row's `started_at` still belongs to the PREVIOUS turn during this
    /// window, and showing it made a fresh send open at the old turn's
    /// half-hour mark.
    pub fn pending_send_started(&self, chat_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.pending_sends
            .get(chat_id)
            .filter(|p| {
                now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
            })
            .map(|p| p.started)
    }

    /// The host executed the queued command iff the sent message's id showed
    /// up in the transcript (it writes the message before — causally with —
    /// the Working status; sessions.rs dispatch paths).
    fn ack_pending_send_from_transcript(&mut self) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(pending) = self.pending_sends.get(chat_id)
            && self.transcript.iter().any(|e| e.id == pending.message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- offline outbox (transparency plan Task 10) ----

    /// Why a send cannot leave right now, in words — `None` when the link is
    /// Ready. The composer queues against this string instead of showing a
    /// button that does nothing; naming the reason is the whole point. Reads
    /// [`Self::link_status`], so a paired environment that is offline blocks
    /// exactly like a dead engine.
    pub fn send_block_reason(&self) -> Option<String> {
        match self.link_status() {
            ConnectionStatus::Ready => None,
            ConnectionStatus::Connecting => Some("connecting".to_string()),
            ConnectionStatus::Reconnecting {
                attempt,
                retry_in_ms,
                ..
            } => Some(format!(
                "reconnecting ({attempt}, {})",
                format_ms(retry_in_ms)
            )),
            ConnectionStatus::Degraded { reason } => Some(format!("link degraded: {reason}")),
            ConnectionStatus::Failed { retryable, .. } => Some(if retryable {
                "engine unreachable — press Retry".to_string()
            } else {
                "engine unreachable".to_string()
            }),
        }
    }

    /// Park a message the link cannot carry, under the chat it was typed in.
    /// Returns the reason shown on the row. Duplicates of the
    /// immediately-preceding entry for the same chat are refused: a user
    /// pressing Return twice in a row while nothing visibly happened must not
    /// queue the same paragraph twice.
    pub fn queue_send(&mut self, chat_key: String, text: String) -> String {
        let reason = self.send_block_reason().unwrap_or_default();
        if self
            .queued_sends
            .last()
            .is_some_and(|last| last.chat_key == chat_key && last.text == text)
        {
            return reason;
        }
        self.queued_sends.push(QueuedSend {
            chat_key,
            text,
            reason,
        });
        self.queued_sends.last().unwrap().reason.clone()
    }

    /// The parked messages, oldest first.
    pub fn queued_sends(&self) -> &[QueuedSend] {
        &self.queued_sends
    }

    /// Hand over the parked messages owned by `chat_key` and leave the other
    /// chats' entries in the outbox. Taking before sending is the dedupe
    /// guarantee: the texts are out of the state before the first send runs,
    /// so a link that dies mid-drain has `send()` re-park that one text once —
    /// nothing replays, nothing is lost.
    pub fn take_queued_sends_for(&mut self, chat_key: &str) -> Vec<String> {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued_sends)
            .into_iter()
            .partition(|queued| queued.chat_key == chat_key);
        self.queued_sends = rest;
        mine.into_iter().map(|queued| queued.text).collect()
    }

    /// The status the shell narrates: the engine link, or — while a paired
    /// environment is displayed — that environment's reachability spoken in the
    /// SAME vocabulary. A paired env that is offline is `Degraded` (this app
    /// works, the thing you are looking at does not), never a dead gate card, and
    /// its rows stay hidden: only the env's own pump can fill them. Pure.
    pub fn link_status(&self) -> ConnectionStatus {
        if self.is_local_workspace() {
            return self.connection.clone();
        }
        match self.active_workspace_status() {
            WorkspaceStatus::Connecting => ConnectionStatus::Connecting,
            WorkspaceStatus::Offline => ConnectionStatus::Degraded {
                reason: format!(
                    "{} is offline",
                    self.workspace_row(&self.active_workspace)
                        .map(|row| row.label.clone())
                        .unwrap_or_else(|| "Environment".to_string())
                ),
            },
            // Reachable environment: the app-level link still decides — a
            // reconnecting engine takes its relay with it.
            WorkspaceStatus::Online => self.connection.clone(),
        }
    }

    /// Attachment upload starting: expose its progress to the working label.
    pub fn begin_upload_progress(
        &mut self,
        total: u64,
        done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.upload_progress = Some(UploadProgress { done, total });
    }

    /// Upload leg over (success or failure) — the label goes back to plain
    /// send/working wording.
    pub fn end_upload_progress(&mut self) {
        self.upload_progress = None;
    }

    /// Percent of the in-flight attachment upload, clamped to 99 — the last
    /// point belongs to the commit + queue, so "100% but still spinning"
    /// never shows. `None` when no upload is in flight (or it's empty).
    pub fn upload_progress_percent(&self) -> Option<u8> {
        let progress = self.upload_progress.as_ref()?;
        if progress.total == 0 {
            return None;
        }
        let done = progress
            .done
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(progress.total);
        Some(((done * 100) / progress.total).min(99) as u8)
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        if self.no_project {
            return None;
        }
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    /// The device the new-session canvas targets: the picked project's host
    /// when one is selected, else the explicit device pick, else this device.
    pub fn effective_device_id(&self) -> Option<String> {
        if let Some(space) = self.selected_space_row() {
            return Some(space.device_id.clone());
        }
        self.selected_device
            .clone()
            .or_else(|| self.local_device_id.clone())
    }

    /// The device a per-machine RPC must name. On this computer the engine
    /// answers for itself and needs no target; inside a paired device the
    /// environment runtime has no agent installs of its own (the controller is a
    /// viewer there), so an untargeted call would describe the wrong machine.
    pub fn harness_target_device(&self) -> Option<String> {
        if self.is_local_workspace() {
            return None;
        }
        self.effective_device_id()
    }

    /// Pick the composer's target device. Keeps the project pick consistent:
    /// a project on another device can't survive the switch — fall back to
    /// the first project on the new device, else "no project".
    pub fn select_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let project_moves = self
            .selected_space_row()
            .is_some_and(|s| s.device_id != device_id);
        if project_moves {
            let first = self
                .spaces_sorted()
                .iter()
                .find(|s| s.device_id == device_id)
                .map(|s| s.id.clone());
            self.no_project = first.is_none();
            if first.is_some() {
                self.selected_space = first;
            }
        }
        self.selected_device = Some(device_id);
        cx.notify();
    }

    /// Record the id this machine answers with for `LocalDevice`. It is always
    /// the home row; it only becomes the *displayed* workspace's device while
    /// the local workspace is up, because the probe can land after a switch and
    /// would otherwise aim a paired device's chats at the controller.
    fn note_home_device(&mut self, id: String) {
        self.home_device_id = Some(id.clone());
        if self.active_workspace == LOCAL_ENV_ID {
            self.local_device_id = Some(id);
        }
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// Spaces in display order — case-insensitive alphabetical, the order
    /// both space selectors (sidebar filter, composer picker) list rows in.
    /// Ties break on id so the order is stable across renders.
    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The machine this app runs on is trivially online; unknown
    /// devices get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.home_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// The "@ device" tag for a space — shared by the space pickers' rows,
    /// the sidebar filter trigger, and the composer's space chip. Returns
    /// `(tag, offline)`; staleness renders as a disconnected GLYPH at the
    /// call sites (user request), never words in the tag.
    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline = !self.device_online(&space.device_id, now);
        let device = self
            .device_name(&space.device_id)
            .unwrap_or("Unknown device");
        (format!("@ {device}"), offline)
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list). A send in
    /// flight ([`Self::begin_pending_send`]) reads as Working — the queued
    /// command is as good as running.
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.send_pending(&chat.id, now) {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE space,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| match c.space_id.as_deref() {
                // Project-less sessions are first-class rows.
                None => true,
                Some(id) => self.space_row(id).is_some(),
            })
            .map(|c| (self.display_status_for(c, now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row. A send in flight reads as
    /// Working (see [`Self::display_status_for`]).
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        if self.send_pending(chat_id, now) {
            return Indicator::Working;
        }
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection, self.workspace_scope, self.auth.as_ref())
    }

    /// The connection the views must use: the displayed workspace's, which is the
    /// engine's own for the local workspace. `None` while a paired device is
    /// being dialed — a remote workspace never falls back to local rows.
    pub fn engine(&self) -> Option<&EngineHandle> {
        if self.active_workspace != LOCAL_ENV_ID {
            // A paired device that is not connected has no connection to borrow.
            // Falling back to the local engine would write the next chat into the
            // wrong workspace.
            return self.display_engine.as_ref();
        }
        self.engine.as_ref()
    }

    /// This machine's engine: lifecycle (mode, shutdown, reconnect) and the
    /// environment RPC surface, which is local-only by protocol.
    pub fn local_engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    pub fn is_local_workspace(&self) -> bool {
        self.active_workspace == LOCAL_ENV_ID
    }

    /// Which connection the watches of the displayed workspace belong to.
    fn watch_owner(&self) -> WatchOwner {
        if self.is_local_workspace() {
            WatchOwner::Engine
        } else {
            WatchOwner::Workspace(self.active_workspace.clone())
        }
    }

    /// A paired device's switcher row (`None` for the local workspace).
    pub fn workspace_row(&self, env_id: &str) -> Option<&WorkspaceEntry> {
        self.workspaces.iter().find(|row| row.id == env_id)
    }

    /// The switcher's list as the sidebar renders it: this machine first, then
    /// every paired device.
    pub fn workspace_switcher(&self) -> Vec<WorkspaceEntry> {
        let mut rows = vec![WorkspaceEntry {
            id: LOCAL_ENV_ID.to_string(),
            label: self.local_workspace_label(),
            host: String::new(),
            status: WorkspaceStatus::Online,
            device_count: self.local_devices.len().max(1),
            version: workspace_version(&self.local_devices, self.home_device_id.as_deref()),
        }];
        rows.extend(self.workspaces.iter().cloned());
        rows
    }

    /// Keep the local workspace label aligned with the device name itself, without
    /// the "This computer" prefix that duplicates the paired-device row below it.
    pub fn local_workspace_label(&self) -> String {
        self.home_device_id
            .as_deref()
            .and_then(|id| self.local_devices.iter().find(|d| d.id == id))
            .map(|d| d.name.clone())
            .or_else(|| self.local_devices.first().map(|d| d.name.clone()))
            .unwrap_or_else(|| "This computer".to_string())
    }

    /// The switcher trigger's label: the workspace being displayed.
    pub fn active_workspace_label(&self) -> String {
        if self.is_local_workspace() {
            return self.local_workspace_label();
        }
        self.workspace_row(&self.active_workspace)
            .map(|row| row.label.clone())
            .unwrap_or_else(|| "Device".to_string())
    }

    /// Reachability of the displayed workspace, for the trigger's dot.
    pub fn active_workspace_status(&self) -> WorkspaceStatus {
        if self.is_local_workspace() {
            return WorkspaceStatus::Online;
        }
        self.workspace_row(&self.active_workspace)
            .map(|row| row.status)
            .unwrap_or(WorkspaceStatus::Connecting)
    }

    /// Drop every account-scoped view and subscription after its runtime has
    /// stopped. The next bootstrap must never render rows from the previous
    /// account while the local profile is opening.
    pub fn prepare_runtime_replacement(&mut self, cx: &mut Context<Self>) {
        self.engine = None;
        self.watch_tasks.clear();
        self.data_tasks.clear();
        self.transcript_task = None;
        self.clear_workspaces();
        self.connection = ConnectionStatus::Connecting;
        self.workspace_scope = None;
        self.auth = None;
        self.devices.clear();
        self.spaces.clear();
        self.chats.clear();
        self.sessions.clear();
        self.selected_space = None;
        self.no_project = false;
        self.selected_device = None;
        self.selected_chat = None;
        self.auto_selected = false;
        self.pending_access = None;
        self.pending_chat_config = None;
        self.sync_access_mode();
        self.chats_synced = false;
        self.spaces_synced = false;
        self.transcript.clear();
        self.active_plan = None;
        self.echoes.clear();
        self.pending_sends.clear();
        self.queued_sends.clear();
        self.reconnect_task = None;
        self.reconnect_attempt = 0;
        self.reconnect_pending = false;
        self.reconnect_deadline = None;
        self.local_device_id = None;
        self.home_device_id = None;
        self.update = None;
        cx.notify();
    }

    // ---- gpui glue ----

    /// What a dropped link is allowed to throw away: the pumps, and the
    /// workspace relays that rode on them — `attach_engine` re-makes both, and
    /// the transcript of the selected chat is re-subscribed there. What this
    /// does NOT touch is what the user is looking at: the transcript, the
    /// echoes and the outbox survive the gap, so a reconnect never blinks
    /// (transparency plan Task 10).
    fn drop_engine_subscriptions(&mut self) {
        self.watch_tasks.clear();
        self.data_tasks.clear();
        // Paired-device connections ride the same daemon, so they go too and
        // re-attach from the next environment frame.
        self.clear_workspaces();
        self.transcript_task = None;
    }

    /// Re-dial the engine after the link dropped, on the BOUNDED backoff chain:
    /// attempt `n` waits `reconnect_base_delay_ms(n)` ±20 % jitter, the ordinal
    /// and the deadline ride on `ConnectionStatus::Reconnecting` so the wait is
    /// countable, and after `RECONNECT_MAX_ATTEMPTS` the chain stops with
    /// `Failed { retryable: true }` — an endless silent redial reads as an app
    /// that answers nothing. Guarded: one attempt runs at a time, and only from
    /// a previously-Remote or Failed state (InProcess engines are already local).
    /// The transcript, the echoes and the outbox survive: only subscriptions
    /// die, and `attach_engine` re-makes them.
    fn schedule_reconnect(&mut self, reason: String, cx: &mut Context<Self>) {
        if self.reconnect_pending || self.connection == ConnectionStatus::Connecting {
            return; // a chain attempt is already queued or dialing
        }
        self.reconnect_pending = true;
        let Some(config) = self.boot_config.clone() else {
            self.reconnect_pending = false;
            self.connection = ConnectionStatus::Failed {
                message: reason,
                retryable: false,
            };
            cx.notify();
            return;
        };
        // Draw the jitter from the clock: two viewports reconnecting into the
        // same daemon must not peak on it at the same millisecond.
        let unit = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| (since.subsec_micros() % 1_000) as f64 / 1_000.0)
            .unwrap_or(0.5);
        let Some((attempt, delay_ms)) = next_reconnect_step(self.reconnect_attempt, unit) else {
            tracing::error!(attempt = self.reconnect_attempt, %reason, "engine reconnect chain exhausted");
            self.reconnect_deadline = None;
            self.connection = ConnectionStatus::Failed {
                message: reason,
                retryable: true,
            };
            cx.notify();
            return;
        };
        self.reconnect_attempt = attempt;
        self.drop_engine_subscriptions();
        self.reconnect_deadline =
            Some(Utc::now() + chrono::Duration::milliseconds(delay_ms as i64));
        self.connection = ConnectionStatus::Reconnecting {
            attempt,
            retry_in_ms: delay_ms,
            reason,
        };
        // Drop the dead engine handle so `attach_engine` can replace it cleanly.
        // Shutdown is best-effort (remote daemon is already gone).
        if let Some(old) = self.engine.take() {
            let old = old;
            cx.spawn(async move |_, _| old.shutdown().await).detach();
        }
        cx.notify();
        // Re-use the existing bootstrap path (probe → remote-or-embed). We
        // need an `App` context, but we only have `Context<Self>` here — spawn
        // via gpui_tokio's `Tokio` handle and then hop back to gpui for the
        // state update, mirroring `bootstrap`.
        let state = cx.entity().clone();
        self.reconnect_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay_ms))
                .await;
            let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            state.update(cx, |s, cx| {
                // The attempt is over either way: release the guard before
                // deciding, or the chain could never advance past attempt 1.
                s.reconnect_pending = false;
                s.reconnect_deadline = None;
                match outcome {
                    Ok(handle) => s.attach_engine(handle, cx),
                    Err(message) => {
                        tracing::warn!(%message, "engine reconnect attempt failed");
                        // Fall through the chain: this attempt is the one that
                        // just failed, so the next call schedules attempt + 1.
                        s.schedule_reconnect(message, cx);
                    }
                }
            });
        }));
    }

    /// The user pressed Retry: the chain restarts from the top of the schedule
    /// (attempt 0 → 500 ms) rather than continuing where it gave up.
    pub fn reset_reconnect_chain(&mut self) {
        self.reconnect_attempt = 0;
        self.reconnect_deadline = None;
    }

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        let boot_config = config.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            // A manual Retry (and a fresh boot) restarts the chain from the top
            // of the schedule, not from where the last one gave up — and
            // cancels whatever attempt was still sleeping, so its outcome
            // cannot replace the new engine or fork a second chain.
            s.reconnect_task = None;
            s.reconnect_attempt = 0;
            s.reconnect_pending = false;
            s.reconnect_deadline = None;
            s.workspace_scope = None;
            s.auth = None;
            s.data_dir = Some(data_dir.clone());
            s.new_chat_access =
                crate::settings::security::SecurityDefaults::load(&data_dir).default_sandbox;
            s.pending_access = None;
            s.sync_access_mode();
            s.boot_config = Some(boot_config);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    // A cold boot gets no auto-retry chain (the user is looking
                    // at the gate card), but it must never be a dead end.
                    s.connection = ConnectionStatus::Failed {
                        message,
                        retryable: true,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        let engine_info = handle.engine_info();
        self.workspace_scope = Some(engine_info.workspace_scope);
        self.local_device_id = Some(engine_info.device_id.clone());
        self.home_device_id = Some(engine_info.device_id.clone());
        self.engine = Some(handle.clone());
        // A daemon restart takes its `?env=` connections with it: the viewport
        // lands back on the local workspace and the environment list re-attaches
        // its own pumps below.
        self.clear_workspaces();
        let mut watch_tasks = Vec::with_capacity(8);
        if let Some(task) = spawn_deferred_engine_watch(cx, handle.clone()) {
            watch_tasks.push(task);
        }
        watch_tasks.extend([
            // Auth frames parse tolerantly — engine and proto tags differ today.
            spawn_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                methods::AUTH_STATUS,
                AppState::apply_auth_value,
            ),
            spawn_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ]);
        self.watch_tasks = watch_tasks;
        self.start_local_data_watches(cx);
        // Paired devices live on this engine: (re)attach their list and their
        // background streams once the handle is in place.
        self.watch_workspaces(cx);
        // EngineInfo is part of the attachment boundary: views must know which
        // data profile they reached before they are allowed to render Ready.
        self.connection = ConnectionStatus::Ready;
        // The chain succeeded (or this is the first boot): the counter restarts
        // from the top of the schedule, and the deadline goes with the banner.
        self.reconnect_attempt = 0;
        self.reconnect_pending = false;
        self.reconnect_deadline = None;
        // Re-subscribe the transcript if a chat was already selected (reconnect path).
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                chat_id,
            ));
        }
        cx.notify();
    }

    /// The local workspace's row watches. Kept separate from [`Self::watch_tasks`]
    /// because they stop while a paired device is displayed.
    fn start_local_data_watches(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.engine.as_ref().map(|handle| handle.client_arc()) else {
            return;
        };
        self.data_tasks = vec![
            spawn_watch(
                cx,
                client.clone(),
                WatchOwner::Engine,
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, client.clone(), WatchOwner::Engine),
            spawn_watch(
                cx,
                client.clone(),
                WatchOwner::Engine,
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                client,
                WatchOwner::Engine,
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
        ];
    }

    // ---- paired-device workspaces ----

    /// Follow the engine's environment list: the switcher's rows, one standing
    /// device stream per paired device, and the cleanup when one is removed.
    fn watch_workspaces(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.engine.as_ref().map(|handle| handle.client_arc()) else {
            return;
        };
        self.workspaces_task = Some(cx.spawn(async move |this, cx| {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            loop {
                let mut rx = match client
                    .subscribe(methods::WATCH_ENVIRONMENTS, serde_json::json!({}))
                    .await
                {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, "environment list unavailable; retrying");
                        if this.update(cx, |_, _| {}).is_err() {
                            return;
                        }
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue;
                    }
                };
                while let Some(value) = rx.recv().await {
                    match serde_json::from_value::<Vec<Environment>>(value) {
                        Ok(envs) => {
                            let _ = this.update(cx, |state, cx| state.apply_environments(envs, cx));
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "dropping malformed environments frame");
                        }
                    }
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor().timer(RETRY_DELAY).await;
            }
        }));
    }

    /// Rebuild the switcher's rows from the engine's list, keep a presence pump
    /// per row, and drop everything a removed device owned.
    fn apply_environments(&mut self, envs: Vec<Environment>, cx: &mut Context<Self>) {
        self.paired_envs = envs
            .iter()
            .filter(|env| env.id != LOCAL_ENV_ID)
            .cloned()
            .collect();
        let mut rows = Vec::new();
        for env in envs {
            if env.id == LOCAL_ENV_ID {
                continue;
            }
            if env.namespace.is_none() {
                // No namespace means the runtime never opened; there is nothing
                // to connect to until the device is paired again.
                continue;
            }
            let status = self
                .workspace_row(&env.id)
                .map(|row| row.status)
                .unwrap_or(WorkspaceStatus::Connecting);
            let devices: &[Device] = self
                .env_devices
                .get(&env.id)
                .map_or(&[] as &[Device], Vec::as_slice);
            let own = workspace_home_device(devices, self.home_device_id.as_deref());
            let version = workspace_version(devices, own.as_deref());
            rows.push(WorkspaceEntry {
                id: env.id,
                label: env.label,
                host: url_host(&env.url),
                status,
                device_count: devices.len(),
                version,
            });
        }
        let listed: HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
        let vanished: Vec<String> = self
            .workspaces
            .iter()
            .map(|row| row.id.clone())
            .filter(|id| !listed.contains(id))
            .collect();
        self.workspaces = rows;
        self.env_list_synced = true;
        for id in &vanished {
            self.forget_workspace(id);
        }
        let ids: Vec<String> = self.workspaces.iter().map(|row| row.id.clone()).collect();
        for id in ids {
            self.ensure_presence(cx, &id);
        }
        if vanished.contains(&self.active_workspace) {
            // The displayed device is unpaired: its rows are nothing now.
            self.switch_workspace(LOCAL_ENV_ID.to_string(), cx);
        }
        cx.notify();
    }

    /// Drop a removed device's connection, pumps, rows and navigation memory.
    fn forget_workspace(&mut self, env_id: &str) {
        self.env_presence.remove(env_id);
        self.env_clients.remove(env_id);
        self.env_devices.remove(env_id);
        self.env_memory.remove(env_id);
        if self.active_workspace == env_id {
            self.stop_workspace_watches();
            self.transcript_task = None;
        }
    }

    /// Forget every paired device. The engine that served them is gone (runtime
    /// replacement or daemon restart); the next environment frame re-attaches.
    fn clear_workspaces(&mut self) {
        let was_remote = self.active_workspace != LOCAL_ENV_ID;
        self.env_presence.clear();
        self.env_clients.clear();
        self.env_devices.clear();
        self.env_memory.clear();
        self.workspaces.clear();
        self.paired_envs.clear();
        self.env_list_synced = false;
        self.workspaces_task = None;
        self.stop_workspace_watches();
        if was_remote {
            self.active_workspace = LOCAL_ENV_ID.to_string();
            self.clear_lists();
        }
    }

    /// One standing device stream per paired device, created on demand. It owns
    /// that environment's connection: the switcher's dot, the workspace's device
    /// rows, and the client its other watches ride all come from this one dial,
    /// and it is the only thing that re-dials a device that went away.
    fn ensure_presence(&mut self, cx: &mut Context<Self>, env_id: &str) {
        if self.env_presence.contains_key(env_id) {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let env_id = env_id.to_string();
        let task_env_id = env_id.clone();
        let task = cx.spawn(async move |this, cx| {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            loop {
                let client = match engine.env_client(&env_id).await {
                    Ok(client) => client,
                    Err(err) => {
                        tracing::debug!(%env_id, error = %err, "device workspace unreachable");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue;
                    }
                };
                match client
                    .subscribe(methods::WATCH_DEVICES, serde_json::json!({}))
                    .await
                {
                    Err(err) => {
                        tracing::debug!(%env_id, error = %err, "device stream unavailable");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            match serde_json::from_value::<Vec<Device>>(value) {
                                Ok(devices) => {
                                    let client = client.clone();
                                    let alive = this.update(cx, |state, cx| {
                                        state.env_clients.insert(env_id.clone(), client);
                                        state.record_workspace_devices(&env_id, devices, cx);
                                    });
                                    if alive.is_err() {
                                        return;
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(%env_id, error = %err, "dropping malformed device frame");
                                }
                            }
                        }
                        tracing::debug!(%env_id, "device stream ended; re-dialing");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(RETRY_DELAY).await;
            }
        });
        self.env_presence.insert(task_env_id, task);
    }

    /// Device rows of one paired workspace. This machine is a controller there,
    /// never a device — so the workspace's own machine becomes the composer's
    /// pick, and [`AppState::local_device_id`] follows it.
    fn record_workspace_devices(
        &mut self,
        env_id: &str,
        devices: Vec<Device>,
        cx: &mut Context<Self>,
    ) {
        self.set_workspace_status(env_id, WorkspaceStatus::Online);
        let own = workspace_home_device(&devices, self.home_device_id.as_deref());
        if let Some(row) = self.workspaces.iter_mut().find(|row| row.id == env_id) {
            row.device_count = devices.len();
            row.version = workspace_version(&devices, own.as_deref());
        }
        self.env_devices.insert(env_id.to_string(), devices.clone());
        if self.active_workspace == env_id {
            let home = own;
            self.apply_devices(devices);
            if self.selected_device.is_none() {
                self.selected_device = home.clone();
            }
            self.local_device_id = home;
            self.attach_workspace_client(env_id, cx);
        }
        cx.notify();
    }

    fn set_workspace_status(&mut self, env_id: &str, status: WorkspaceStatus) {
        if let Some(row) = self.workspaces.iter_mut().find(|row| row.id == env_id) {
            row.status = status;
        }
    }

    /// The device stream said the connection is gone: show it in the switcher and
    /// stop speaking through it. The rows stay as a read-only mirror — the
    /// presence pump re-dials, the local workspace is untouched.
    fn handle_workspace_lost(&mut self, env_id: &str, cx: &mut Context<Self>) {
        self.env_clients.remove(env_id);
        self.set_workspace_status(env_id, WorkspaceStatus::Offline);
        if self.active_workspace == env_id {
            self.stop_workspace_watches();
            self.transcript_task = None;
        }
        cx.notify();
    }

    /// Display another workspace. Every row list is cleared first: not one line
    /// of the outgoing workspace may render under the incoming device's label.
    pub fn switch_workspace(&mut self, env_id: String, cx: &mut Context<Self>) {
        let is_local = env_id == LOCAL_ENV_ID;
        if self.active_workspace == env_id && (is_local || self.display_engine.is_some()) {
            return;
        }
        let outgoing = self.active_workspace.clone();
        if outgoing != LOCAL_ENV_ID {
            self.env_memory.insert(
                outgoing,
                WorkspaceMemory {
                    selected_space: self.selected_space.clone(),
                    selected_device: self.selected_device.clone(),
                    selected_chat: self.selected_chat.clone(),
                    no_project: self.no_project,
                },
            );
        }
        self.stop_workspace_watches();
        self.data_tasks.clear();
        self.transcript_task = None;
        self.active_workspace = env_id.clone();
        self.clear_lists();
        if is_local {
            self.local_device_id = self.home_device_id.clone();
            self.start_local_data_watches(cx);
        } else {
            // Warm rows mean the composer can target that machine before its
            // first frame lands; the watches start with the connection.
            let devices = self.env_devices.get(&env_id).cloned().unwrap_or_default();
            self.local_device_id = workspace_home_device(&devices, self.home_device_id.as_deref());
            self.selected_device = self.local_device_id.clone();
            if let Some(memory) = self.env_memory.get(&env_id) {
                self.selected_space = memory.selected_space.clone();
                self.selected_device = memory
                    .selected_device
                    .clone()
                    .or_else(|| self.local_device_id.clone());
                self.selected_chat = memory.selected_chat.clone();
                self.no_project = memory.no_project;
            }
            self.attach_workspace_client(&env_id, cx);
        }
        cx.notify();
    }

    fn clear_lists(&mut self) {
        self.chats.clear();
        self.spaces.clear();
        self.devices.clear();
        self.sessions.clear();
        self.transcript.clear();
        self.active_plan = None;
        self.context_usage.clear();
        self.echoes.clear();
        self.pending_sends.clear();
        self.review_comments.clear();
        self.upload_progress = None;
        self.workspace_state = None;
        self.selected_chat = None;
        self.selected_space = None;
        self.selected_device = None;
        self.no_project = false;
        self.auto_selected = false;
        self.chats_synced = false;
        self.spaces_synced = false;
        self.pending_access = None;
        self.pending_chat_config = None;
        self.sync_access_mode();
    }

    /// Aim the display at a paired device's connection, once it exists.
    fn attach_workspace_client(&mut self, env_id: &str, cx: &mut Context<Self>) {
        if self.active_workspace != env_id || self.display_engine.is_some() {
            return;
        }
        let Some(client) = self.env_clients.get(env_id).cloned() else {
            return;
        };
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let engine_info = EngineInfo {
            device_id: self.local_device_id.clone().unwrap_or_default(),
            workspace_scope: engine.engine_info().workspace_scope,
        };
        let owner = WatchOwner::Workspace(env_id.to_string());
        self.display_engine = Some(EngineHandle::for_workspace(
            client.clone(),
            engine_info,
            engine.mode(),
        ));
        self.env_watches = vec![
            spawn_watch(
                cx,
                client.clone(),
                owner.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, client.clone(), owner.clone()),
            spawn_watch(
                cx,
                client.clone(),
                owner.clone(),
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
        ];
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, client, owner, chat_id));
        }
    }

    fn stop_workspace_watches(&mut self) {
        self.env_watches.clear();
        self.display_engine = None;
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        // Each chat uses its own level; a fresh new-chat screen uses the
        // Security default (drop any leftover canvas click).
        self.pending_access = None;
        self.sync_access_mode();
        self.transcript.clear();
        self.active_plan = None;
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its project (or the lack of one); `select_chat(None)`
            // (the new-session canvas) keeps the current project pick.
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                match chat.space_id.clone() {
                    Some(space_id) => {
                        self.selected_space = Some(space_id);
                        self.no_project = false;
                    }
                    None => {
                        self.no_project = true;
                        self.selected_device = Some(chat.device_id.clone());
                    }
                }
            }
            self.mark_chat_seen(id, cx);
        }
        if let Some(chat_id) = chat_id
            && let Some(client) = self.engine().map(|handle| handle.client_arc())
        {
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                client,
                self.watch_owner(),
                chat_id,
            ));
        }
        cx.notify();
    }

    /// Select a project; the caller (shell) decides which chat to land on.
    /// `Some` clears a "Don't work in a project" opt-out and re-aims the
    /// device pick at the project's host; `None` IS that opt-out.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        match &space_id {
            Some(id) => {
                self.no_project = false;
                if let Some(device) = self.space_row(id).map(|s| s.device_id.clone()) {
                    self.selected_device = Some(device);
                }
            }
            None => self.no_project = true,
        }
        if self.selected_space == space_id && space_id.is_some() {
            cx.notify();
            return;
        }
        if space_id.is_some() {
            self.selected_space = space_id;
        }
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    /// Window-focus liveness sweep: ask the engine to probe every open room
    /// (workspace + chat docs). Fire-and-forget; each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so spamming is harmless.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.local_engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({});
            if let Err(err) = handle.client().call(methods::PROBE_SYNC, params).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine().cloned() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }

    // ---- file editor helpers ----

    /// Read a file's text content from the local filesystem.
    pub fn load_file_content(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    /// Write text content to a file on the local filesystem.
    pub fn save_file_content(&self, path: &str, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| e.to_string())
    }
}

/// Observe assembly after an early attach (cloud onboarding or another viewport
/// reaching the embedded engine over IPC). Data subscriptions wait on the same
/// result, but their individual errors are not authoritative: older engines may
/// legitimately omit a watch method. Only the assembly result may fail the
/// whole connection.
fn spawn_deferred_engine_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
) -> Option<Task<()>> {
    let mut deferred = handle.deferred_state()?;
    Some(cx.spawn(async move |this, cx| {
        let Err(failure) = wait_for_deferred_engine(&mut deferred).await else {
            return;
        };
        tracing::error!(error = %failure, "engine assembly failed after attachment");
        // Embedded handles release their IPC listener before exposing Retry;
        // remote handles stop their completed readiness probe.
        handle.shutdown().await;
        this.update(cx, |state, cx| {
            // The runtime died after attaching: say so and hand Retry to the
            // user — re-embedding a half-assembled engine automatically would
            // hide a real startup failure behind a spinner.
            state.connection = ConnectionStatus::Failed {
                message: failure,
                retryable: true,
            };
            cx.notify();
        })
        .ok();
    }))
}

fn is_connection_error(err: &RpcError) -> bool {
    matches!(err, RpcError::Closed | RpcError::Transport(_))
}

/// The sandbox level for the `selected` chat. The new-chat screen (no
/// selection) uses `default`. An existing chat uses its saved level
/// (`ChatConfig.sandbox`, synced across devices), or `WorkspaceWrite`, the
/// level such chats always used, when it has no saved configuration or its
/// row has not loaded yet. Changing the default therefore never raises the
/// level of an existing chat.
pub fn access_for_selection(
    chats: &[Chat],
    selected: Option<&str>,
    default: komet_proto::SandboxLevel,
) -> komet_proto::SandboxLevel {
    let Some(id) = selected else {
        return default;
    };
    chats
        .iter()
        .find(|c| c.id == id)
        .and_then(|chat| chat.config.as_ref())
        .map_or(komet_proto::SandboxLevel::WorkspaceWrite, |config| {
            config.sandbox
        })
}

/// Chats watch. Boot selection is the shell's job (it lands on the first
/// restored open tab, device-local state this entity can't see); this task
/// only pumps frames.
fn spawn_chats_watch(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop (same contract as the transcript watch): a daemon
        // restart or RPC drop ends the stream, and a bare return here froze
        // the sidebar until app restart — new chats, renames and archives
        // from every device silently stopped arriving.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match client
                .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(error = %err, "chats watch unavailable; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: Vec<Chat> = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping malformed chats frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    state.apply_chats(parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!("chats stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Report a dropped connection for the watch's owner and stop this pump: whoever
/// re-dials re-attaches it. A workspace pump must never reconnect through the
/// local engine — its rows would arrive under the wrong device.
fn pump_lost(this: &gpui::WeakEntity<AppState>, owner: &WatchOwner, cx: &mut gpui::AsyncApp) {
    match owner {
        WatchOwner::Engine => {
            let _ = this.update(cx, |state, cx| {
                tracing::warn!("engine connection lost; reconnecting");
                state.schedule_reconnect("engine connection lost".to_string(), cx);
            });
        }
        WatchOwner::Workspace(env_id) => {
            let env_id = env_id.clone();
            let _ = this.update(cx, |state, cx| {
                tracing::warn!(%env_id, "device workspace connection lost");
                state.handle_workspace_lost(&env_id, cx);
            });
        }
    }
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop: these are the standing Sessions/Devices/Spaces
        // watches — a daemon restart ended the stream and a bare return froze
        // them for the rest of the app's life (remote Working dots staled out
        // to nothing after 45s, and Idle/Completed transitions from other
        // devices never arrived again — "the session never completes").
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match client.subscribe(method, serde_json::json!({})).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(method, error = %err, "watch unavailable; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: T = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(method, error = %err, "dropping malformed watch frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    apply(state, parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!(method, "watch stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Best-effort `LocalDevice` probe: names the registry row that is "This
/// device". Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.note_home_device(id);
                cx.notify();
            })
            .ok();
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    client: Arc<RpcClient>,
    owner: WatchOwner,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop — a return here freezes the transcript
        // with no banner and no heal short of an app restart (this watch and
        // its engine-side room are the ONLY transcript delivery path). The
        // task itself is dropped by select_chat/apply_chats when the chat is
        // deselected or deleted, so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match client.subscribe(methods::WATCH_DOC_MESSAGES, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if is_connection_error(&err) {
                        pump_lost(&this, &owner, cx);
                        return;
                    }
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use komet_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `komet_proto::view`.
    use komet_proto::{SessionStatus, UserProfile};

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    struct LegacyIdentityRpc;

    #[async_trait]
    impl RpcService for LegacyIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::LOCAL_DEVICE => {
                    RpcReply::value(&serde_json::json!({ "deviceId": "legacy-device" }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    struct DeferredIdentityRpc {
        engine_info: EngineInfo,
        state: tokio::sync::watch::Receiver<DeferredEngineState>,
    }

    #[async_trait]
    impl RpcService for DeferredIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
                methods::ENGINE_READY => {
                    let mut state = self.state.clone();
                    wait_for_deferred_engine(&mut state)
                        .await
                        .map_err(RpcError::Failed)?;
                    RpcReply::value(&serde_json::json!({ "ready": true }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn legacy_daemon_identity_remains_usable() {
        let client = memory_client(Arc::new(LegacyIdentityRpc));

        let info = query_engine_info(&client).await.unwrap();

        assert_eq!(info.device_id, "legacy-device");
        assert_eq!(info.workspace_scope, WorkspaceScope::Synced);
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(info.workspace_scope),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
    }

    /// Publish an engine token for a test server the way `serve_ipc` does, so
    /// the viewport's attach path can read it. The file is removed on drop.
    fn publish_engine_token(port: u16) -> komet_rpc::ipc_token::PublishedToken {
        komet_rpc::ipc_token::publish(&komet_rpc::ipc_token::default_dir().unwrap(), port).unwrap()
    }

    #[tokio::test]
    async fn remote_viewport_treats_legacy_daemon_as_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let published = publish_engine_token(port);
        let server = tokio::spawn(komet_rpc::serve_ws_listener(
            listener,
            Arc::new(LegacyIdentityRpc),
            published.token().into(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .expect("legacy daemon remains attachable");

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks readiness");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deferred_engine(&mut deferred),
        )
        .await
        .expect("legacy readiness fallback completes")
        .expect("unknown EngineReady means the old daemon is assembled");

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(matches!(
            handle
                .deferred_state()
                .expect("embedded lifecycle")
                .borrow()
                .clone(),
            DeferredEngineState::Ready
        ));
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    /// A device connection is built from gpui's main thread, which has no tokio
    /// reactor. The transport has to land on the engine's runtime anyway — the
    /// thread that asks for a device is not the thread that runs it.
    #[tokio::test]
    async fn env_client_builds_a_connection_without_an_ambient_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .unwrap();

        let dialer = {
            let handle = handle.clone();
            std::thread::spawn(move || futures::executor::block_on(handle.env_client(LOCAL_ENV_ID)))
        };
        let client = match dialer.join() {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => panic!("device dial from a runtime-free thread failed: {err}"),
            Err(_) => panic!("the device dial panicked on a thread without a reactor"),
        };
        // The answer travels back through the test runtime; the server task the
        // dial spawned lives on the engine's runtime the whole time.
        let harnesses = client
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await;
        let harnesses = match harnesses {
            Ok(harnesses) => harnesses,
            Err(err) => panic!("environment connection built without a reactor: {err}"),
        };
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_reports_local_assembly_failure_before_returning_a_handle() {
        let dir = tempfile::tempdir().unwrap();
        // Komet is local-first: the boot resolves the DEVELOPMENT profile, so
        // the store corruption must sit on that profile's device root.
        std::fs::create_dir_all(dir.path().join("orgs")).unwrap();
        std::fs::write(dir.path().join("orgs/dev-org"), b"not a directory").unwrap();
        let port = free_port().await;

        let error = match EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: Some("client_test".into()),
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        {
            Ok(handle) => {
                handle.shutdown().await;
                panic!("a corrupt local store must fail bootstrap")
            }
            Err(error) => error,
        };

        assert!(!format!("{error:#}").is_empty());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "failed bootstrap must release the IPC listener"
        );
    }

    #[tokio::test]
    async fn deferred_engine_failure_remains_observable_after_early_attach() {
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));

        assert_eq!(
            wait_for_deferred_engine(&mut state_rx).await,
            Err("store failed".into())
        );
    }

    #[tokio::test]
    async fn remote_viewport_observes_deferred_engine_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let published = publish_engine_token(port);
        let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let server = tokio::spawn(komet_rpc::serve_ws_listener(
            listener,
            Arc::new(DeferredIdentityRpc {
                engine_info: EngineInfo {
                    device_id: "owner-device".into(),
                    workspace_scope: WorkspaceScope::Local,
                },
                state: state_rx,
            }),
            published.token().into(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .expect("second viewport attaches over IPC");
        assert!(matches!(handle.mode(), EngineMode::Remote { .. }));

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks engine readiness");
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("remote readiness probe completes"),
            Err("store failed".into())
        );

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let token = komet_rpc::ipc_token::read_default(port).unwrap();
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"), &token)
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn concurrent_bootstraps_elect_one_embedded_engine() {
        // Two viewports of one app booting at once (the Local-switch restart
        // path): both used to probe a closed port, both embedded, and one lost
        // the data-dir lock. The bootstrap gate must elect exactly one owner
        // and turn the other into a plain remote attach.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let config = EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        };
        let (a, b) = tokio::join!(
            EngineHandle::bootstrap(config.clone()),
            EngineHandle::bootstrap(config.clone()),
        );
        let a = a.expect("first viewport boots");
        let b = b.expect("second viewport boots");

        let modes = [a.mode(), b.mode()];
        assert_eq!(
            modes
                .iter()
                .filter(|mode| **mode == EngineMode::InProcess)
                .count(),
            1,
            "exactly one viewport embeds: {modes:?}"
        );
        assert_eq!(
            modes
                .iter()
                .filter(|mode| matches!(mode, EngineMode::Remote { .. }))
                .count(),
            1,
            "the other attaches over IPC: {modes:?}"
        );

        for handle in [&a, &b] {
            let mut deferred = handle.deferred_state().expect("lifecycle tracked");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("readiness resolves")
            .expect("both viewports reach Ready");
        }

        b.shutdown().await;
        a.shutdown().await;
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn production_bootstrap_opens_local_data_without_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: Some("client_test".into()),
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .unwrap();

        // Komet is local-first: without a synced backend the boot resolves the
        // development profile (dev identity, no sign-in gate).
        assert_eq!(
            handle.engine_info().workspace_scope,
            WorkspaceScope::Development
        );
        let info: EngineInfo = handle
            .client()
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(info, *handle.engine_info());

        let mut auth = handle
            .client()
            .subscribe(methods::AUTH_STATUS, serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(
            parse_auth_state(&auth.recv().await.unwrap()),
            Some(AuthState::SignedIn { .. })
        ));
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .expect("local data RPC is immediately available");
        assert!(harnesses.as_array().is_some_and(|items| !items.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `komet headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let published = publish_engine_token(port);
        tokio::spawn(komet_rpc::serve_ws_listener(
            listener,
            core.rpc_service(),
            published.token().into(),
        ));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            sync_token: None,
            default_harness: HarnessId::Mock,
            managed_engine: false,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        assert_eq!(
            handle.engine_info().workspace_scope,
            WorkspaceScope::Development
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        assert!(matches!(
            handle
                .client()
                .call(methods::STOP_ENGINE, serde_json::json!({}))
                .await,
            Err(RpcError::Failed(message))
                if message == format!("unknown method: {}", methods::STOP_ENGINE)
        ));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
            context_usage: None,
        }
    }

    fn user_entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: komet_doc::MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn local_workspace_hides_the_unknown_device_sentinel() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.local_device_id = Some("local".into());

        state.apply_devices(vec![
            device("local", "unknown-device"),
            device("remote", "unknown-device"),
        ]);

        assert_eq!(state.device_name("local"), Some("Local"));
        assert_eq!(state.device_name("remote"), Some("unknown-device"));

        state.apply_devices(vec![device("local", "José's MacBook Pro")]);
        assert_eq!(state.device_name("local"), Some("José's MacBook Pro"));
    }

    #[test]
    fn workspace_host_is_what_the_switcher_shows() {
        assert_eq!(
            url_host("https://vm1.example.com:8443/"),
            "vm1.example.com:8443"
        );
        assert_eq!(url_host("http://192.168.1.20:8787"), "192.168.1.20:8787");
        assert_eq!(url_host("not a url"), "");
    }

    #[test]
    fn a_paired_workspace_targets_its_own_machine_not_the_controller() {
        let devices = vec![device("vm1", "vm1"), device("other", "other")];
        assert_eq!(
            workspace_home_device(&devices, Some("fedora")).as_deref(),
            Some("vm1")
        );
        // The controller's row is not in this workspace: fall back to any device.
        assert_eq!(
            workspace_home_device(&[device("vm1", "vm1")], Some("vm1")).as_deref(),
            Some("vm1")
        );
        assert_eq!(workspace_home_device(&[], Some("vm1")), None);
    }

    #[test]
    fn only_the_machine_under_the_app_is_trivially_online() {
        let now = Utc::now();
        let mut state = AppState::new();
        state.home_device_id = Some("fedora".into());
        // `local_device_id` follows the displayed workspace, so it must not be
        // the trivially-online answer: vm1's staleness is real information.
        state.local_device_id = Some("vm1".into());
        state.devices = vec![device("vm1", "vm1")];
        assert!(state.device_online("fedora", now));
        assert!(!state.device_online("vm1", now));
    }

    #[test]
    fn a_late_local_device_answer_never_retargets_a_paired_workspace() {
        let mut state = AppState::new();
        state.active_workspace = "env-1".into();
        state.local_device_id = Some("vm1".into());
        state.note_home_device("fedora".into());
        assert_eq!(state.home_device_id.as_deref(), Some("fedora"));
        assert_eq!(
            state.local_device_id.as_deref(),
            Some("vm1"),
            "the controller's id must not become the displayed device's target"
        );

        state.active_workspace = LOCAL_ENV_ID.into();
        state.note_home_device("fedora".into());
        assert_eq!(state.local_device_id.as_deref(), Some("fedora"));
    }

    #[test]
    fn a_row_reports_the_version_of_the_machine_under_it() {
        let mut vm1 = device("vm1", "vm1");
        vm1.version = Some("1.4.0".into());
        let mut laptop = device("fedora", "fedora");
        laptop.version = Some("1.3.0".into());

        // Inside a paired workspace the controller has no registry row, so the
        // row's version is that machine's — never the app's own.
        assert_eq!(
            workspace_version(&[vm1.clone()], Some("fedora")).as_deref(),
            Some("1.4.0")
        );
        assert_eq!(
            workspace_version(&[laptop, vm1], Some("fedora")).as_deref(),
            Some("1.3.0")
        );
        assert_eq!(workspace_version(&[], Some("fedora")), None);
    }

    #[test]
    fn send_pending_overlays_working_until_ttl() {
        let now = Utc::now();
        let s_chat = chat("c", 0, Some(10)); // unseen, no session row
        let mut s = AppState::new();
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Completed);
        assert_eq!(s.indicator_for("c", now), Indicator::None);
        s.begin_pending_send("c", "m1", now);
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Working);
        assert_eq!(s.indicator_for("c", now), Indicator::Working);
        // Time-bounded: an offline host must not leave an eternal spinner.
        let later = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
        assert_eq!(
            s.display_status_for(&s_chat, later),
            ChatIndicator::Completed
        );
        assert_eq!(s.indicator_for("c", later), Indicator::None);
    }

    #[test]
    fn send_pending_acked_when_the_host_writes_the_message_back() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.selected_chat = Some("c".into());
        s.begin_pending_send("c", "m1", now);
        // A frame without the message keeps the overlay.
        s.apply_transcript(vec![user_entry("other")]);
        assert!(s.send_pending("c", now));
        // The host executed the command: our id comes back in the doc.
        s.apply_transcript(vec![user_entry("other"), user_entry("m1")]);
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn send_failure_cleanup_only_ends_its_own_overlay() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.begin_pending_send("c", "m1", now);
        s.begin_pending_send("c", "m2", now); // quick resend superseded m1
        s.end_pending_send("c", "m1"); // m1's failure cleanup arrives late
        assert!(s.send_pending("c", now), "m2's overlay must survive");
        s.end_pending_send("c", "m2");
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn apply_sessions_stamps_context_usage_for_the_ring() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let mut sess = session("c1", SessionStatus::Working, 0, Utc::now());
        sess.context_usage = Some(komet_proto::ContextUsageStats::window(
            1_000,
            10_000,
            komet_proto::ContextUsageSource::Native,
            900,
            0,
            100,
            0,
        ));
        state.apply_sessions(vec![sess]);
        let stats = state.current_context_usage();
        assert_eq!(stats.used(), 1_000);
        assert_eq!(stats.source, komet_proto::ContextUsageSource::Native);
    }

    #[test]
    fn approximate_billed_overshoot_does_not_pin_the_ring_at_full() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        state.transcript = vec![komet_doc::SessionMessageEntry {
            id: "m1".into(),
            role: komet_doc::MessageRole::User,
            parts: vec![komet_doc::MessagePart::Text {
                id: "p1".into(),
                text: "x".repeat(40_000),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        }];
        state.apply_context_usage(
            "c1",
            komet_proto::ContextUsageStats::window(
                131_072,
                131_072,
                komet_proto::ContextUsageSource::Approximate,
                1_800_000,
                0,
                24_000,
                0,
            ),
        );
        let stats = state.current_context_usage();
        assert_eq!(stats.source, komet_proto::ContextUsageSource::Estimated);
        assert_eq!(stats.used(), 10_000);
        assert!(stats.context_percent() < 100);
        assert_eq!(stats.input_tokens, 1_800_000);
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-space chat (idle included) PLUS
        // project-less chats (first-class since the project selectors);
        // chats of unknown spaces stay hidden. Completed ("old") outranks
        // idle ("new"/"dangling").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new", "dangling"]);
    }

    fn chat_at(id: &str, sandbox: komet_proto::SandboxLevel) -> Chat {
        let mut row = chat(id, 0, None);
        row.config = Some(komet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            sandbox,
            mcp_server_ids: Vec::new(),
        });
        row
    }

    // Scenario 4: an existing chat uses its own level, not the default.
    #[test]
    fn access_for_selection_prefers_the_chats_own_level() {
        use komet_proto::SandboxLevel::*;
        let chats = vec![chat_at("ro", ReadOnly), chat("legacy", 1, None)];
        // New-chat screen: the default.
        assert_eq!(
            access_for_selection(&chats, None, DangerFullAccess),
            DangerFullAccess
        );
        // Existing chat with a saved level: its own level.
        assert_eq!(
            access_for_selection(&chats, Some("ro"), DangerFullAccess),
            ReadOnly
        );
        // An existing chat without a saved configuration keeps the level such
        // chats always used, so a Full access default never reaches it.
        assert_eq!(
            access_for_selection(&chats, Some("legacy"), DangerFullAccess),
            WorkspaceWrite
        );
        // Same for a selected chat whose row has not loaded yet; the level is
        // recomputed when the row arrives.
        assert_eq!(
            access_for_selection(&chats, Some("gone"), DangerFullAccess),
            WorkspaceWrite
        );
    }

    #[test]
    fn access_mode_follows_selection_and_remote_level_changes() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();
        state.new_chat_access = DangerFullAccess;
        state.apply_chats(vec![chat_at("a", ReadOnly)]);
        // No chat open: the default.
        assert_eq!(state.access_mode, DangerFullAccess);
        state.selected_chat = Some("a".into());
        state.sync_access_mode();
        assert_eq!(state.access_mode, ReadOnly);
        // Another device changes the chat's level: the new level applies.
        state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
        assert_eq!(state.access_mode, WorkspaceWrite);
        // A local change to the open chat's configuration applies too.
        state.apply_chat_config("a", chat_at("a", ReadOnly).config.unwrap());
        assert_eq!(state.access_mode, ReadOnly);
        // The open chat is deleted on another device: back to the default.
        state.apply_chats(Vec::new());
        assert_eq!(state.access_mode, DangerFullAccess);
    }

    #[test]
    fn new_chat_access_chip_survives_chats_watch_frames() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();
        state.new_chat_access = WorkspaceWrite;
        state.apply_chats(vec![chat("a", 0, None)]);
        assert_eq!(state.access_mode, WorkspaceWrite);
        state.set_access_mode(DangerFullAccess);
        assert_eq!(state.access_mode, DangerFullAccess);
        // A sidebar refresh must not snap Full access back to Sandboxed.
        state.apply_chats(vec![chat("a", 0, None)]);
        assert_eq!(state.access_mode, DangerFullAccess);
    }

    /// Regression (2026-09-21): the composer must snapshot `access_mode`
    /// BEFORE calling `select_chat` on a newly-minted chat id.
    ///
    /// `select_chat` clears `pending_access` and calls `sync_access_mode`.
    /// For a brand-new chat whose row has not yet arrived from the engine the
    /// sync falls back to `new_chat_access` (WorkspaceWrite by default),
    /// silently downgrading a Full Access first run even when the user had
    /// explicitly clicked the chip before sending.
    #[test]
    fn access_mode_snapshot_before_select_chat_preserves_full_access() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();

        // User clicks the access chip to Full access on the new-chat canvas.
        state.set_access_mode(DangerFullAccess);
        assert_eq!(state.access_mode, DangerFullAccess);

        // CORRECT (fixed) order: snapshot THEN select.
        let sandbox_before = state.access_mode;

        // Simulate select_chat with a brand-new UUID that has no row yet:
        // it clears pending_access and syncs (falling back to new_chat_access).
        state.selected_chat = Some("new-uuid".into());
        state.pending_access = None;
        state.sync_access_mode(); // resolves to new_chat_access = WorkspaceWrite

        assert_eq!(state.access_mode, WorkspaceWrite); // row hasn't landed yet
        assert_eq!(sandbox_before, DangerFullAccess); // snapshot is still correct

        // BAD (old) order: snapshot AFTER select — simulates the pre-fix bug.
        state.selected_chat = None;
        state.pending_access = None;
        state.sync_access_mode();
        state.set_access_mode(DangerFullAccess);
        assert_eq!(state.access_mode, DangerFullAccess);

        state.selected_chat = Some("new-uuid-2".into());
        state.pending_access = None;
        state.sync_access_mode(); // access_mode reset to WorkspaceWrite

        let sandbox_after = state.access_mode; // taken AFTER — was the bug
        assert_eq!(
            sandbox_after, WorkspaceWrite,
            "old order reads WorkspaceWrite instead of DangerFullAccess"
        );
    }

    #[test]
    fn access_chip_survives_watch_when_chat_config_is_not_saved() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None)]);
        state.selected_chat = Some("a".into());
        state.sync_access_mode();
        assert_eq!(state.access_mode, WorkspaceWrite);
        // Unknown harness: the click cannot be written, but the next message
        // still uses this level — a watch frame must not snap it back.
        state.set_access_mode(DangerFullAccess);
        state.apply_chats(vec![chat("a", 0, None)]);
        assert_eq!(state.access_mode, DangerFullAccess);
    }

    #[test]
    fn set_chat_config_echo_survives_a_stale_chats_frame() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();
        state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
        state.selected_chat = Some("a".into());
        state.sync_access_mode();
        assert_eq!(state.access_mode, WorkspaceWrite);
        state.apply_chat_config("a", chat_at("a", DangerFullAccess).config.unwrap());
        assert_eq!(state.access_mode, DangerFullAccess);
        // Watch frame that left before the mutate landed.
        state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
        assert_eq!(state.access_mode, DangerFullAccess);
        // Engine caught up: the echo is released.
        state.apply_chats(vec![chat_at("a", DangerFullAccess)]);
        assert_eq!(state.access_mode, DangerFullAccess);
        // A later remote change is not stuck behind the echo.
        state.apply_chats(vec![chat_at("a", ReadOnly)]);
        assert_eq!(state.access_mode, ReadOnly);
    }

    #[test]
    fn discarded_chat_config_echo_lets_the_watch_win() {
        use komet_proto::SandboxLevel::*;
        let mut state = AppState::new();
        state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
        state.selected_chat = Some("a".into());
        state.sync_access_mode();
        state.apply_chat_config("a", chat_at("a", DangerFullAccess).config.unwrap());
        state.discard_pending_chat_config();
        // Local stamp still shows the click until a watch frame arrives.
        assert_eq!(state.access_mode, DangerFullAccess);
        state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
        assert_eq!(state.access_mode, WorkspaceWrite);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = komet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(komet_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
            mcp_server_ids: Vec::new(),
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            komet_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
                mcp_server_ids: Vec::new(),
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: komet_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    /// Fold a cycle's frames the way the pipeline does: `inProgress` then
    /// `completed` land on ONE part (refined in place by compaction id), so a
    /// thread never holds two parts for one boundary.
    fn compaction_parts(
        compaction_id: &str,
        frames: &[(komet_proto::CompactionStatus, Option<&str>)],
    ) -> Vec<komet_doc::MessagePart> {
        let mut parts = Vec::new();
        for (status, trigger) in frames {
            komet_doc::fold_event_into_parts(
                &mut parts,
                &komet_proto::AgentEvent::ContextCompaction {
                    id: compaction_id.into(),
                    status: *status,
                    trigger: trigger.map(str::to_owned),
                    tokens_before: Some(165_000),
                    tokens_after: None,
                    summary: None,
                    summary_delta: None,
                    error: None,
                },
            );
        }
        parts
    }

    fn compaction_entry(
        id: &str,
        frames: &[(komet_proto::CompactionStatus, Option<&str>)],
    ) -> SessionMessageEntry {
        SessionMessageEntry {
            id: format!("sys-{id}"),
            role: komet_doc::MessageRole::System,
            parts: compaction_parts(id, frames),
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn thread_compactions_reads_the_doc_when_the_agent_says_nothing() {
        use komet_proto::CompactionStatus::{Completed, InProgress};
        let mut state = AppState::new();
        assert_eq!(state.thread_compactions(), ThreadCompactions::default());
        // A cycle in flight is not a boundary yet — the popover must not count
        // it, only mark it live.
        state.transcript = vec![compaction_entry("c1", &[(InProgress, None)])];
        let mid = state.thread_compactions();
        assert_eq!((mid.count, mid.in_progress), (0, true));
        assert_eq!(mid.last_trigger, None, "no reason was ever reported");
        // The same cycle settling refines its part (it is not a second
        // boundary), and a second cycle adds one — with the newest reported
        // reason winning.
        state.transcript = vec![
            compaction_entry("c1", &[(InProgress, None), (Completed, Some("auto"))]),
            compaction_entry("c2", &[(Completed, Some("manual"))]),
        ];
        let done = state.thread_compactions();
        assert_eq!(done.count, 2, "two cycles, two parts");
        assert!(!done.in_progress, "the refined part is no longer in flight");
        assert_eq!(done.last_trigger.as_deref(), Some("manual"));
    }

    fn plan_entry(id: &str, items: Vec<komet_proto::TodoItem>) -> SessionMessageEntry {
        let mut parts = Vec::new();
        komet_doc::fold_event_into_parts(
            &mut parts,
            &komet_proto::AgentEvent::ToolCall {
                id: "acp-plan".into(),
                call: komet_proto::ToolCall::Todo { items },
            },
        );
        SessionMessageEntry {
            id: id.into(),
            role: komet_doc::MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn active_plan_tracks_the_latest_todo_call() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let first = vec![komet_proto::TodoItem {
            text: "a".into(),
            done: false,
        }];
        let last = vec![komet_proto::TodoItem {
            text: "a".into(),
            done: true,
        }];
        state.apply_transcript(vec![
            plan_entry("m1", first),
            plan_entry("m2", last.clone()),
        ]);
        assert_eq!(state.active_plan, Some(last));
    }

    #[test]
    fn active_plan_follows_in_place_delta_frames() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let first = vec![komet_proto::TodoItem {
            text: "a".into(),
            done: false,
        }];
        state.apply_transcript(vec![plan_entry("m1", first.clone())]);
        assert_eq!(state.active_plan, Some(first));

        // The streaming case: the live entry is replaced in place, so the
        // transcript keeps one entry and the scan must still run — the plan
        // card is driven by exactly this frame.
        let last = vec![komet_proto::TodoItem {
            text: "a".into(),
            done: true,
        }];
        state
            .apply_transcript_frame(TranscriptFrame::Delta {
                upsert: vec![komet_doc::TranscriptUpsert {
                    after: None,
                    entry: plan_entry("m1", last.clone()),
                }],
                append: vec![],
                remove: vec![],
                count: 1,
            })
            .expect("in-place plan frame");
        assert_eq!(state.active_plan, Some(last));

        // Removing the plan entry changes the length, which is the other
        // trigger: the cache must not stay on a snapshot no entry carries.
        state
            .apply_transcript_frame(TranscriptFrame::Delta {
                upsert: vec![],
                append: vec![],
                remove: vec!["m1".into()],
                count: 0,
            })
            .expect("plan entry removed");
        assert_eq!(state.active_plan, None);
    }

    #[test]
    fn active_plan_clears_when_its_entry_is_truncated() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        state.apply_transcript(vec![plan_entry(
            "m1",
            vec![komet_proto::TodoItem {
                text: "a".into(),
                done: false,
            }],
        )]);
        assert!(state.active_plan.is_some());
        state.truncate_transcript_from("c1", "m1");
        assert_eq!(state.active_plan, None);
        // A later full snapshot without a plan also clears it.
        state.apply_transcript(vec![plan_entry(
            "m2",
            vec![komet_proto::TodoItem {
                text: "a".into(),
                done: true,
            }],
        )]);
        state.apply_transcript(vec![]);
        assert_eq!(state.active_plan, None);
    }

    #[test]
    fn gate_phases() {
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Failed {
                    message: "boom".into(),
                    retryable: false
                },
                None,
                None
            ),
            GatePhase::Failed("boom".into())
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Local),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, Some(WorkspaceScope::Synced), None),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut)
            ),
            GatePhase::Ready
        );
    }

    #[test]
    fn connection_backoff_is_bounded_and_visible() {
        // The whole chain, driven with jitter switched off (unit 0.5 → no
        // displacement): 500 ms → 1 s → 2 s → 5 s → 10 s, saturating, and it
        // STOPS. An unbounded redial loop is the "spinner that answers nothing"
        // this task exists to remove.
        let mut attempt = 0u32;
        let mut chain = Vec::new();
        while let Some((next, delay)) = next_reconnect_step(attempt, 0.5) {
            chain.push((next, delay));
            attempt = next;
        }
        assert_eq!(
            chain,
            vec![
                (1, 500),
                (2, 1_000),
                (3, 2_000),
                (4, 5_000),
                (5, 10_000),
                (6, 10_000),
            ],
            "six attempts, the last two at the ceiling"
        );
        assert_eq!(attempt, RECONNECT_MAX_ATTEMPTS);

        // ±20 %, clamped: a bad jitter source cannot leave the band.
        assert_eq!(reconnect_jittered_delay_ms(1_000, 0.0), 800);
        assert_eq!(reconnect_jittered_delay_ms(1_000, 1.0), 1_200);
        assert_eq!(reconnect_jittered_delay_ms(1_000, 7.0), 1_200);

        // The wait is EXPOSED while it runs, not just scheduled: the ordinal and
        // the deadline are what the banner counts down against, and the gate
        // stays open (no white screen) for as long as the chain has links left.
        let mut state = AppState::new();
        state.connection = ConnectionStatus::Reconnecting {
            attempt: 3,
            retry_in_ms: 2_000,
            reason: "engine connection lost".into(),
        };
        state.reconnect_attempt = 3;
        state.reconnect_deadline = Some(Utc::now() + chrono::Duration::milliseconds(2_000));
        assert_eq!(state.reconnect_attempt, 3);
        assert!(state.reconnect_deadline.is_some());
        assert_eq!(gate_phase(&state.connection, None, None), GatePhase::Ready);

        // Ceiling reached: the chain hands the Retry button to the user, and
        // `retryable: true` is what keeps that button from being a lie.
        assert_eq!(next_reconnect_step(RECONNECT_MAX_ATTEMPTS, 0.5), None);
        state.connection = ConnectionStatus::Failed {
            message: "engine connection lost".into(),
            retryable: true,
        };
        assert!(matches!(
            state.connection,
            ConnectionStatus::Failed {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn reconnect_keeps_the_last_frame_and_the_echoes() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        state.connection = ConnectionStatus::Ready;
        state.transcript = vec![user_entry("m1"), user_entry("m2")];
        state.push_echo("c1", user_entry("m3"));
        state.watch_tasks.push(Task::ready(()));
        state.data_tasks.push(Task::ready(()));
        state.transcript_task = Some(Task::ready(()));
        state.workspaces = vec![WorkspaceEntry {
            id: "env-1".into(),
            label: "Work".into(),
            host: "sync.example".into(),
            status: WorkspaceStatus::Online,
            device_count: 1,
            version: None,
        }];

        // What the dropped link throws away is the pumps — nothing the user is
        // reading. (`attach_engine` re-makes them and re-subscribes the
        // transcript of the still-selected chat.)
        state.drop_engine_subscriptions();
        assert!(state.watch_tasks.is_empty());
        assert!(state.data_tasks.is_empty());
        assert!(state.transcript_task.is_none());
        assert!(state.workspaces.is_empty());
        assert_eq!(state.transcript.len(), 2, "the last frame survives the gap");
        assert_eq!(
            state.pending_echoes().len(),
            1,
            "an unconfirmed echo survives too"
        );
        assert_eq!(state.selected_chat.as_deref(), Some("c1"));

        // And the state it lands in keeps the shell up rather than revealing a
        // gate card over an empty canvas.
        state.connection = ConnectionStatus::Reconnecting {
            attempt: 1,
            retry_in_ms: 500,
            reason: "engine connection lost".into(),
        };
        assert_eq!(gate_phase(&state.connection, None, None), GatePhase::Ready);
    }

    #[test]
    fn paired_environment_reconnect_uses_the_same_states() {
        let mut state = AppState::new();
        state.connection = ConnectionStatus::Ready;
        let work = WorkspaceEntry {
            id: "env-1".into(),
            label: "Work".into(),
            host: "sync.example".into(),
            status: WorkspaceStatus::Online,
            device_count: 1,
            version: None,
        };

        // Local workspace: the engine link IS the status.
        assert_eq!(state.link_status(), ConnectionStatus::Ready);
        state.connection = ConnectionStatus::Reconnecting {
            attempt: 2,
            retry_in_ms: 1_000,
            reason: "engine connection lost".into(),
        };
        assert!(matches!(
            state.link_status(),
            ConnectionStatus::Reconnecting { attempt: 2, .. }
        ));

        // A paired environment speaks the SAME vocabulary — the banner needs no
        // second code path — and never borrows the local engine's health to
        // look alive: that is how one environment's rows could show up as
        // another's.
        state.active_workspace = "env-1".into();
        state.workspaces = vec![work.clone()];
        state.connection = ConnectionStatus::Ready;
        state.workspaces[0].status = WorkspaceStatus::Connecting;
        assert_eq!(state.link_status(), ConnectionStatus::Connecting);
        state.workspaces[0].status = WorkspaceStatus::Offline;
        assert_eq!(
            state.link_status(),
            ConnectionStatus::Degraded {
                reason: "Work is offline".into()
            }
        );
        // Reachable environment: the engine's own state shows through again.
        // Keep the reconnect banner alive until the runtime has actually
        // recovered, then drop back to Ready as soon as the engine reports it.
        state.workspaces[0].status = WorkspaceStatus::Online;
        state.connection = ConnectionStatus::Reconnecting {
            attempt: 2,
            retry_in_ms: 1_000,
            reason: "engine connection lost".into(),
        };
        assert!(matches!(
            state.link_status(),
            ConnectionStatus::Reconnecting { .. }
        ));
        state.connection = ConnectionStatus::Ready;
        assert_eq!(state.link_status(), ConnectionStatus::Ready);
    }

    #[test]
    fn auth_changes_do_not_change_a_local_runtime_scope_or_watches() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.watch_tasks.push(Task::ready(()));

        state.apply_auth(AuthState::NeedsOrganization {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);

        state.apply_auth(AuthState::SignedIn {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);
    }

    #[test]
    fn auth_frames_parse_both_wire_shapes() {
        // Proto shape.
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        // Engine shape (`_tag`, PascalCase, orgId).
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "orgId": "org-1",
        });
        let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(org_id.as_deref(), Some("org-1"));
        let needs = serde_json::json!({
            "_tag": "NeedsOrganization",
            "user": { "id": "u1", "email": "w@example.com", "name": "W" },
        });
        assert!(matches!(
            parse_auth_state(&needs),
            Some(AuthState::NeedsOrganization { .. })
        ));
        // Garbage → None (frame dropped, not a crash).
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/komet")), "komet");
        assert_eq!(project_label(Some("/home/w/dev/komet/")), "komet");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/komet")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/komet")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["komet", "zed", "No project"]);
        let komet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(komet_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_komet_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("komet/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · komet/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn org_gate_reducers() {
        assert!(org_name_valid("Acme"));
        assert!(org_name_valid("  padded  "));
        assert!(!org_name_valid(""));
        assert!(!org_name_valid("   "));
        assert!(!org_name_valid(&"x".repeat(65)));

        let rows = parse_orgs(&serde_json::json!({ "orgs": [
            { "id": "m2", "organizationId": "o2", "name": "beta" },
            { "id": "m1", "organizationId": "o1", "name": "Alpha" },
            { "id": "m3", "organizationId": "o1", "name": "Alpha" },
        ]}));
        assert_eq!(rows.len(), 3);
        let sorted = sort_memberships(rows);
        let names: Vec<&str> = sorted.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            ["Alpha", "beta"],
            "case-insensitive sort + dedupe by org id"
        );
        // Bare-array replies parse too; garbage yields empty.
        assert_eq!(
            parse_orgs(&serde_json::json!([{ "id": "m", "organizationId": "o", "name": "n" }]))
                .len(),
            1
        );
        assert!(parse_orgs(&serde_json::json!("nope")).is_empty());
    }
}
