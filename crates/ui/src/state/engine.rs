use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use komet_engine::{
    Engine, EngineConfig, EngineCore, EngineRuntime, InstanceLock, environments::LOCAL_ENV_ID,
    rpc::AuthRpc,
};
use komet_proto::{Device, EngineInfo, HarnessId, WorkspaceScope};
use komet_rpc::{
    RpcClient, RpcError, RpcReply, RpcService, ServiceRouter, connect_ws, connect_ws_for,
    memory_client, methods,
};

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
pub(crate) enum WatchOwner {
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
pub(crate) struct WorkspaceMemory {
    pub(crate) selected_space: Option<String>,
    pub(crate) selected_device: Option<String>,
    pub(crate) selected_chat: Option<String>,
    pub(crate) no_project: bool,
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
pub(crate) fn workspace_home_device(devices: &[Device], home: Option<&str>) -> Option<String> {
    devices
        .iter()
        .find(|device| Some(device.id.as_str()) != home)
        .or_else(|| devices.first())
        .map(|device| device.id.clone())
}

/// The engine version of the machine a workspace runs on, from its own registry
/// row — `None` while no row has landed.
pub(crate) fn workspace_version(devices: &[Device], own_id: Option<&str>) -> Option<String> {
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
pub(crate) enum DeferredEngineState {
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

pub(crate) async fn wait_for_deferred_engine(
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

        // On Linux, the managed engine often fails to bind IPC within the timeout.
        // Default to embedded for faster startup; managed can be enabled via env var.
        #[cfg(target_os = "linux")]
        let use_managed =
            config.managed_engine && std::env::var("KOMET_USE_MANAGED_ENGINE").is_ok();
        #[cfg(not(target_os = "linux"))]
        let use_managed = config.managed_engine;

        if use_managed && let Some(handle) = Self::start_managed_engine(&config).await {
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
                "IPC port is held by a listener that is not this komet's engine;                  embedding this window instead of starting a managed engine"
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
            // A new session, not just a new process group: the child must not
            // keep the launching terminal as its controlling tty. Otherwise its
            // login-shell PATH probe (`bash -lic`) touches `/dev/tty`, the
            // background group is stopped by SIGTTOU, and the engine never
            // reaches assembly. It also makes "detached" true: closing the
            // terminal that launched the app no longer hangs up the engine.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
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
        // Reduced from 180s to 5s because the embedded engine assembles in ~16s,
        // and if the managed engine takes longer we should fall back to embedded.
        const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
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
    pub(crate) fn for_workspace(
        client: Arc<RpcClient>,
        engine_info: EngineInfo,
        mode: EngineMode,
    ) -> Self {
        EngineHandle {
            inner: Arc::new(WorkspaceEngine { client, mode }),
            engine_info,
            deferred_state: None,
        }
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    pub(crate) fn deferred_state(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<DeferredEngineState>> {
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
pub(crate) async fn query_engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
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
                #[allow(dead_code)]
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
