//! Meta's Muse Code agent, driven natively over the Muse Session Protocol
//! (MSP): `muse serve` speaking newline-delimited JSON-RPC 2.0 on stdio —
//! the protocol Muse's own SDK drives, with no adapter in between.
//!
//! - One host process per Komet run, one session in it ([`session`]).
//!   The session is resumed by id on the next runtime; one the host no
//!   longer has starts fresh with a visible notice.
//! - Turns end on the host's `turn/completed`, including the turns Muse
//!   starts itself when background subagents finish, so the turn end is
//!   deterministic. Steers ride `turn/start` with `ifBusy: "steer"`, a stop
//!   is `turn/interrupt` and leaves the host (and its background agents)
//!   running.
//! - Model discovery runs on a separate read-only host
//!   (`--disable-shell --disable-write --no-session-log`).
//! - The host's environment drops an inherited `META_API_KEY` (so the
//!   user's own Muse login is used) and turns off Muse's self-update.
mod catalog;
mod host;
mod normalize;
mod rpc;
mod session;

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use komet_proto::{AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{Harness, HarnessError, RunControls};
use host::{Host, Mode};

/// The Muse Code harness. Construct with [`MuseHarness::new`]; tests point it
/// at a fake CLI with [`MuseHarness::with_executable`].
pub struct MuseHarness {
    executable: Option<PathBuf>,
    models_cache: tokio::sync::Mutex<Option<Vec<Model>>>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

impl Default for MuseHarness {
    fn default() -> Self {
        Self {
            executable: None,
            models_cache: tokio::sync::Mutex::new(None),
            interrupt_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl MuseHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune how long a teardown waits for Muse to confirm the stop, and the
    /// SIGTERM→SIGKILL grace after it.
    pub fn with_graces(mut self, interrupt: Duration, kill: Duration) -> Self {
        self.interrupt_grace = interrupt;
        self.kill_grace = kill;
        self
    }

    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = self
            .executable
            .clone()
            .or_else(|| std::env::var_os("MUSE_EXECUTABLE").map(PathBuf::from))
        {
            if path.is_file() {
                return Ok(path);
            }
            return Err(HarnessError::NotInstalled(format!(
                "Muse Code CLI not found at {}; set MUSE_EXECUTABLE to its path",
                path.display()
            )));
        }
        let mut candidates = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|dir| dir.join("muse"))
            .collect::<Vec<_>>();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            candidates.push(home.join(".local/bin/muse"));
        }
        candidates.extend([
            PathBuf::from("/opt/homebrew/bin/muse"),
            PathBuf::from("/usr/local/bin/muse"),
        ]);
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                HarnessError::NotInstalled(
                "Muse Code CLI: install it (curl -fsSL https://dev.meta.ai/install.sh | sh) or \
                 set MUSE_EXECUTABLE"
                    .into(),
            )
            })
    }

    /// `model/list` from a short-lived read-only host.
    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.resolve_executable()?;
        let cwd = std::env::current_dir()?;
        let mut host = Host::spawn(&exe, &cwd, Mode::ReadOnly, &Default::default())?;
        let grace = self.kill_grace;
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            host.initialize(true).await?;
            host.call("model/list", json!({}), &mut Vec::new())
                .await?
                .map_err(|failure| host::failed("model/list", &failure))
        })
        .await
        .unwrap_or_else(|_| {
            Err(HarnessError::Protocol(
                "Muse model discovery timed out".into(),
            ))
        });
        host.shutdown(grace).await;
        let models = catalog::models(&result?);
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "Muse returned no models; run `muse` and sign in with /login".into(),
            ));
        }
        Ok(models)
    }

    fn start(
        &self,
        request: RunRequest,
        controls: RunControls,
        read_only: bool,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mode = if read_only {
            Mode::ReadOnly
        } else {
            Mode::Session {
                sandbox: request.sandbox != komet_proto::SandboxLevel::DangerFullAccess,
            }
        };
        let host = Host::spawn(&exe, Path::new(&request.cwd), mode, &Default::default())?;
        let (tx, rx) = mpsc::channel(256);
        let kill_grace = self.kill_grace;
        let interrupt_grace = self.interrupt_grace;
        tokio::spawn(async move {
            let mut runner = session::Runner::new(host, tx, request, read_only);
            // The lease lives through shutdown even if the consumer drops its stream.
            let RunControls {
                request_input,
                request_permission,
                mut steering,
                interrupt,
            } = controls;
            runner.input = Some(std::sync::Arc::from(request_input));
            runner.permission = Some(std::sync::Arc::from(request_permission));
            let consumer = runner.tx.clone();
            let result = tokio::select! {
                result = runner.run(&mut steering) => result,
                _ = interrupt.cancelled() => { runner.interrupt(interrupt_grace).await; Ok(()) },
                _ = consumer.closed() => Ok(()),
            };
            if let Err(error) = result {
                let status = runner.host.child.try_wait().ok().flatten();
                runner.error = Some(if status.is_some() {
                    crate::crash_message("Muse", status, &runner.host.tail)
                } else {
                    error.to_string()
                });
            }
            runner.host.shutdown(kill_grace).await;
            let _ = runner.finish().await;
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
                .boxed(),
        )
    }
}

#[async_trait]
impl Harness for MuseHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Muse
    }
    fn display_name(&self) -> &str {
        "Muse Code"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// `turn/steer` lands at the running turn's next model step.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    /// Tiers are per model (`model/list` variants).
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let mut cached = self.models_cache.lock().await;
        if let Some(models) = cached.as_ref() {
            return Ok(models.clone());
        }
        let models = self.discover_models().await?;
        *cached = Some(models.clone());
        Ok(models)
    }
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "muse-spark-1.2".into(),
            label: "Muse Spark 1.2".into(),
            description: Some("Muse Code's default coding model".into()),
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            options: vec![],
        }]
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.start(request, controls, false)
    }
}
