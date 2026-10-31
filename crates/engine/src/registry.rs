//! HarnessRegistry — the engine's harness catalog: eager instances (mock) plus lazy
//! slots resolved on first use (claude-code spawns subprocess discovery; codex/cursor
//! later). Lazy slots carry a static descriptor so `ListHarnesses` never forces a spawn.
//!
//! Also owns the device's harness ENABLEMENT (Settings → Agents): which harnesses
//! this device's composer offers, persisted in `{data_dir}/harness-prefs.json`.
//! Per-device because CLI installs are — a viewer retargets the settings page at
//! another device and edits THAT device's set over the forwarded RPCs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use komet_harness::{Harness, HarnessError, mock::MockHarness};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, HarnessInteractionSurface, Model, ReasoningLevel,
    SteeringMode,
};

/// A served cache entry is re-probed in the background at most this often, so
/// the file stays warm without a probe per picker open. There is no upper
/// bound on serving age: a known list is always better than a spinner, and a
/// failing agent still shows its last known models until a probe replaces
/// them.
const MODELS_CACHE_REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);

/// Startup grace before any background model discovery runs. The probe CLIs
/// are heavy (Node/Rust boots); on a small machine they starve the UI's
/// GPU/window init when launched the instant the engine comes up. The picker
/// already has the persisted cache, so nothing is lost by waiting a little.
pub(crate) const BOOT_MODEL_GRACE: Duration = Duration::from_secs(30);

/// One harness's persisted model discovery (`{data_dir}/model-cache.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedModels {
    discovered_at_ms: i64,
    models: Vec<Model>,
}

/// The persisted shape of `model-cache.json`, keyed by harness id.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ModelsCacheFile {
    entries: HashMap<HarnessId, CachedModels>,
}

/// Best-effort atomic write (temp + rename): persist `entries` only — callers
/// hold a successfully probed, non-empty list. Shared by the inline persist
/// and the background refresh so both paths are atomic.
fn persist_models_cache_to(path: &Path, entries: &HashMap<HarnessId, CachedModels>) {
    let file = ModelsCacheFile {
        entries: entries.clone(),
    };
    let json = match serde_json::to_string_pretty(&file) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(error = %err, "model-cache serialize failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path)) {
        tracing::warn!(error = %err, "model-cache save failed");
    }
}

/// What `ListHarnesses` reports per harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// Whether the agent's CLI is present on the listing device (the settings
    /// enable-gate). Defaults true so catalogs from engines predating the
    /// field never read as uninstallable.
    #[serde(default = "default_installed")]
    pub installed: bool,
    /// Whether the listing device offers this harness (Settings → Agents).
    /// `None` — the catalog came from an engine predating the setting — means
    /// "unknown": consumers fall back to [`default_enabled`] membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// What this harness's adapter can show (transparency plan Task 0), derived
    /// from the harness table — the descriptor never declares it a second time.
    /// `Default` (all-absent) decodes a catalog from an engine predating the
    /// field as *unknown*, which no live harness declares.
    #[serde(default)]
    pub surface: HarnessInteractionSurface,
}

fn default_installed() -> bool {
    true
}

/// The out-of-the-box enabled set: Claude Code and Codex only; every other
/// harness is opt-in from Settings → Agents.
pub fn default_enabled() -> Vec<HarnessId> {
    vec![HarnessId::ClaudeCode, HarnessId::Codex]
}

/// A descriptor's effective enabled flag ([`default_enabled`] membership when
/// the catalog predates the setting).
pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor
        .enabled
        .unwrap_or_else(|| default_enabled().contains(&descriptor.id))
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
        installed: harness.installed(),
        enabled: None,
        surface: komet_harness::capabilities::interaction_surface(harness.id()),
    }
}

/// The persisted shape of `harness-prefs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HarnessPrefsFile {
    /// `None` = the user never touched the setting → the default set.
    enabled: Option<Vec<HarnessId>>,
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;
type InstalledProbe = Box<dyn Fn() -> bool + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        /// Re-run on every `descriptors()` call — a CLI installed mid-session
        /// shows up on the next settings/picker open, no restart needed.
        installed: InstalledProbe,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
    /// This device's enabled set; `None` inner value = the default set.
    prefs: Mutex<HarnessPrefsFile>,
    /// Where the prefs persist; `None` (tests, bare registries) skips writes.
    prefs_path: Mutex<Option<PathBuf>>,
    /// Per-harness model discovery, loaded from disk at boot so the picker
    /// renders instantly instead of re-probing every agent on every launch.
    models_cache: Arc<Mutex<HashMap<HarnessId, CachedModels>>>,
    /// Where the model cache persists; `None` (tests, bare registries) skips
    /// disk reads/writes and `models()` always probes.
    models_cache_path: Mutex<Option<PathBuf>>,
    /// Harnesses currently being warmed — `warm_models_cache` claims an id
    /// here so concurrent warms never probe the same harness twice.
    warm_inflight: Mutex<HashSet<HarnessId>>,
    /// When this registry was built — background probes wait for
    /// [`BOOT_MODEL_GRACE`] so window init is not starved by agent CLIs.
    booted_at: std::time::Instant,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            prefs: Mutex::new(HarnessPrefsFile::default()),
            prefs_path: Mutex::new(None),
            models_cache: Arc::new(Mutex::new(HashMap::new())),
            models_cache_path: Mutex::new(None),
            warm_inflight: Mutex::new(HashSet::new()),
            booted_at: std::time::Instant::now(),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn prefs(&self) -> MutexGuard<'_, HarnessPrefsFile> {
        self.prefs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load `harness-prefs.json` from the engine data dir and remember the
    /// path for writes. Corrupt/missing files fall back to the default set.
    pub fn load_prefs(&self, data_dir: &Path) {
        let path = data_dir.join("harness-prefs.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HarnessPrefsFile>(&text).ok())
            .unwrap_or_default();
        *self.prefs() = loaded;
        *self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
    }

    /// Load `model-cache.json` from the engine data dir (alongside the
    /// harness prefs) and remember the path for writes. Corrupt/missing
    /// files fall back to an empty cache — the first `models()` call probes.
    pub fn load_models_cache(&self, data_dir: &Path) {
        let path = data_dir.join("model-cache.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ModelsCacheFile>(&text).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        *self
            .models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = loaded;
        *self
            .models_cache_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
    }

    fn models_cache_entries(&self) -> MutexGuard<'_, HashMap<HarnessId, CachedModels>> {
        self.models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Best-effort atomic write (temp + rename, the prefs pattern).
    fn persist_models_cache(&self) {
        let Some(path) = self
            .models_cache_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        persist_models_cache_to(&path, &self.models_cache_entries());
    }

    /// The model list for `id`.
    ///
    /// `force == false` is the picker's normal path and is CACHE-FIRST: any
    /// known list is served instantly however old it is, and a stale one is
    /// re-probed in the background — so opening the picker, or switching back
    /// to an agent, never re-runs a cold CLI probe. Background work is held
    /// back for [`BOOT_MODEL_GRACE`] after the engine starts. With nothing
    /// cached yet, one live probe runs; if the CLI is installed but failing
    /// (crash, timeout, signed out), the harness's curated
    /// [`Harness::fallback_models`] stands in for the display while the probe
    /// is retried in the background. The send path still refuses ids the CLI
    /// does not advertise, so a fallback row can never start a dead run.
    ///
    /// `force == true` (Rescan / Retry) bypasses the cache entirely and probes
    /// live: a failure propagates and is never masked.
    pub async fn models(&self, id: HarnessId, force: bool) -> Result<Vec<Model>, HarnessError> {
        if force {
            return self.probe_live(id).await;
        }
        let now = crate::now_ms();
        let refresh_after_ms = MODELS_CACHE_REFRESH_AFTER.as_millis() as i64;

        // Cache-first off the boot-loaded cache: the entry serves WITHOUT
        // resolving the harness, so the picker renders before any agent boots
        // (opencode's cold Node start measured ~13s under the app's 4-way boot
        // prefetch; user report: "model loading is too slow"). A stale entry is
        // revalidated off the hot path.
        if let Some(cached) = self.models_cache_entries().get(&id).cloned()
            && !cached.models.is_empty()
        {
            // Stale-entry revalidation also waits out the startup grace: this
            // path is hit by the UI's boot-time prefetch, and probing there
            // would reintroduce the very contention the grace exists to avoid.
            // The boot warm pass revalidates every harness once the grace ends.
            if now - cached.discovered_at_ms >= refresh_after_ms
                && self.booted_at.elapsed() >= BOOT_MODEL_GRACE
            {
                self.spawn_background_refresh(id);
            }
            return Ok(cached.models);
        }

        // Nothing known yet: one blocking probe, then the curated fallback.
        let harness = self.resolve(id)?;
        match harness.models().await {
            Ok(models) if !models.is_empty() => {
                self.store_models(id, now, models.clone());
                Ok(models)
            }
            Ok(_) => {
                // An empty wire answer is never a successful load (every
                // harness errors instead; this arm is the belt).
                let error = HarnessError::Protocol(format!(
                    "{id:?} model discovery returned no usable models"
                ));
                self.fallback_or_error(id, &harness, error)
            }
            Err(error) => self.fallback_or_error(id, &harness, error),
        }
    }

    /// Persist a successfully probed list (success-only, atomically).
    fn store_models(&self, id: HarnessId, now: i64, models: Vec<Model>) {
        self.models_cache_entries().insert(
            id,
            CachedModels {
                discovered_at_ms: now,
                models,
            },
        );
        self.persist_models_cache();
    }

    /// No cache AND a failed probe: show the harness's curated catalog when it
    /// has one, and keep retrying in the background. A harness without a
    /// catalog has nothing honest to show, so its error propagates.
    fn fallback_or_error(
        &self,
        id: HarnessId,
        harness: &Arc<dyn Harness>,
        error: HarnessError,
    ) -> Result<Vec<Model>, HarnessError> {
        let fallback = harness.fallback_models();
        if fallback.is_empty() {
            return Err(error);
        }
        tracing::warn!(
            harness = ?id,
            error = %error,
            "model probe failed with nothing cached; showing the curated fallback and retrying in the background"
        );
        self.spawn_background_refresh(id);
        Ok(fallback)
    }

    /// Re-probe `id` in the background and persist a successful answer
    /// atomically. Failures are silent — the served entry stays until the next
    /// refresh window or an explicit `force` probe.
    fn spawn_background_refresh(&self, id: HarnessId) {
        let Ok(harness) = self.resolve(id) else {
            return;
        };
        let cache = Arc::clone(&self.models_cache);
        let path = self
            .models_cache_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        tokio::spawn(async move {
            if let Ok(fresh) = harness.refresh_models().await
                && !fresh.is_empty()
            {
                cache.lock().unwrap_or_else(PoisonError::into_inner).insert(
                    id,
                    CachedModels {
                        discovered_at_ms: crate::now_ms(),
                        models: fresh,
                    },
                );
                if let Some(path) = path {
                    persist_models_cache_to(
                        &path,
                        &cache.lock().unwrap_or_else(PoisonError::into_inner),
                    );
                }
            }
        });
    }

    /// Pure live probe for `id`: no cache read, no stale fallback — the
    /// harness error propagates. A non-empty answer persists (success-only).
    pub async fn probe_live(&self, id: HarnessId) -> Result<Vec<Model>, HarnessError> {
        let harness = self.resolve(id)?;
        let models = harness.refresh_models().await?;
        if models.is_empty() {
            return Err(HarnessError::Protocol(format!(
                "{id:?} model discovery returned no usable models"
            )));
        }
        self.models_cache_entries().insert(
            id,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: models.clone(),
            },
        );
        self.persist_models_cache();
        Ok(models)
    }

    /// Drop one harness's cached models; the next `models()` probes live.
    pub fn invalidate_models(&self, id: HarnessId) {
        self.models_cache_entries().remove(&id);
        self.persist_models_cache();
    }

    /// Harness ids holding a non-empty cached model list.
    pub fn cached_model_ids(&self) -> Vec<HarnessId> {
        self.models_cache_entries()
            .iter()
            .filter_map(|(id, cached)| (!cached.models.is_empty()).then_some(*id))
            .collect()
    }

    /// Refresh `ids` from their CLIs on every engine start, two harnesses at
    /// a time. Existing cached rows remain available until a successful live
    /// probe replaces them. Single-flight per harness: an id already being
    /// warmed is skipped. Failures are logged and leave the prior cache intact.
    pub async fn warm_models_cache(&self, ids: Vec<HarnessId>) {
        for chunk in ids.chunks(2) {
            if chunk.len() == 2 {
                tokio::join!(self.warm_one(chunk[0]), self.warm_one(chunk[1]));
            } else if let [only] = chunk {
                self.warm_one(*only).await;
            }
        }
    }

    async fn warm_one(&self, id: HarnessId) {
        {
            let mut inflight = self
                .warm_inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !inflight.insert(id) {
                return;
            }
        }
        if let Err(error) = self.probe_live(id).await {
            tracing::warn!(
                harness = ?id,
                error = %error,
                "startup live model refresh failed; retaining cached models"
            );
        }
        self.warm_inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// The enabled set in effect (the default set until the user edits it).
    pub fn enabled_set(&self) -> Vec<HarnessId> {
        self.prefs().enabled.clone().unwrap_or_else(default_enabled)
    }

    /// Whether this device's CLI probe passes for `id` (no spawn, no resolve).
    fn installed_for(&self, id: HarnessId) -> bool {
        match self.slots().get(&id) {
            Some(Slot::Ready(harness)) => harness.installed(),
            Some(Slot::Lazy { installed, .. }) => installed(),
            None => false,
        }
    }

    /// Flip one harness's enablement and persist. Refuses unknown harnesses,
    /// enabling one whose CLI is missing (the settings gate, enforced where
    /// the state lives), and disabling the last enabled harness.
    pub fn set_enabled(&self, id: HarnessId, on: bool) -> Result<(), String> {
        if !self.slots().contains_key(&id) {
            return Err(format!("unknown harness {id:?}"));
        }
        if on && !self.installed_for(id) {
            return Err(format!("{id:?} CLI is not installed on this device"));
        }
        let mut set = self.enabled_set();
        match (on, set.contains(&id)) {
            (true, false) => set.push(id),
            (false, true) => {
                if set.len() == 1 {
                    return Err("cannot disable the last enabled harness".into());
                }
                set.retain(|h| *h != id);
            }
            _ => return Ok(()),
        }
        self.prefs().enabled = Some(set);
        self.persist_prefs();
        Ok(())
    }

    /// Best-effort atomic write (temp + rename, the ui-settings pattern).
    fn persist_prefs(&self) {
        let Some(path) = self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let json = match serde_json::to_string_pretty(&*self.prefs()) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "harness-prefs serialize failed");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %err, "harness-prefs save failed");
        }
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is
    /// cached). `installed` is the CLI-presence probe run per `descriptors()`
    /// call; it must never spawn.
    pub fn register_lazy(
        &self,
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    ) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor,
                    installed,
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    /// Catalog for `ListHarnesses` — never forces a lazy resolve.
    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let enabled = self.enabled_set();
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| {
                let mut descriptor = match slots.get(id) {
                    Some(Slot::Ready(harness)) => describe(harness.as_ref()),
                    Some(Slot::Lazy {
                        descriptor,
                        installed,
                        ..
                    }) => HarnessDescriptor {
                        installed: installed(),
                        ..descriptor.clone()
                    },
                    None => return None,
                };
                descriptor.enabled = Some(enabled.contains(id));
                Some(descriptor)
            })
            .collect()
    }
}

/// The mock harness's script for a headless/dev run. `KOMET_MOCK_SCENARIO`
/// selects a transparency scenario (`compaction`, `subagent_nested`) so those
/// facts are reachable in the offline demo with no live agent and no token;
/// anything else — unset, empty, or an unknown name — runs the default
/// streaming demo `fallback` builds.
fn mock_script(fallback: impl FnOnce() -> Vec<AgentEvent>) -> Vec<AgentEvent> {
    match std::env::var("KOMET_MOCK_SCENARIO")
        .unwrap_or_default()
        .as_str()
    {
        "compaction" => komet_harness::mock::compaction_script(),
        "subagent_nested" => komet_harness::mock::subagent_nested_script(),
        _ => fallback(),
    }
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `komet_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it).
pub fn default_registry() -> HarnessRegistry {
    // Warm the login-shell PATH snapshot in the background so the first
    // claude/codex resolve doesn't pay the shell-startup latency inline.
    komet_harness::shell_env::prewarm();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: mock_script(|| vec![
            AgentEvent::TextDelta {
                text: "## Streaming pipeline\n\nEvery turn flows through the same path:\n\n".into(),
            },
            AgentEvent::TextDelta {
                text: "1. **Doc command** — the composer queues a durable `run` entry\n2. **Host executor** — the chat's host device marks it processed, then dispatches\n3. **Fold** — events fold into parts and diff into the Loro doc every 120ms\n\n".into(),
            },
            AgentEvent::ToolCall {
                id: "mock-tool-1".into(),
                call: komet_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: komet_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
                output: None,
                diff: None,
            },
            AgentEvent::TextDelta {
                text: "The `SegmentWriter` appends into `LoroText` so the oplog stays RLE-merged:\n\n```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms coalesced commits\n```\n\nSynced to every device through the session room. *Mock harness reporting in.*".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
reason: None,
            },
        ]),
    }));
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            // Must mirror the native Claude harness exactly — the
            // descriptor-stability rule (see the Codex test below).
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::ClaudeCode),
        },
        Box::new(|| komet_harness::ClaudeHarness::new().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::ClaudeHarness::new()) as Arc<dyn Harness>)),
    );
    // Codex, same lazy pattern: the static descriptor mirrors CodexHarness
    // exactly (`describe()` after the first resolve must not change the
    // catalog entry) — "Codex" per the original HARNESS_LABEL, StepBoundary
    // steering via native `turn/steer`, and the unified reasoning ladder from
    // komet_harness::codex::catalog. CLI discovery only happens when a
    // run/model call actually resolves the slot.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Codex,
            name: "Codex".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
                ReasoningLevel::Ultra,
            ],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Codex),
        },
        Box::new(|| komet_harness::CodexHarness::new().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::CodexHarness::new()) as Arc<dyn Harness>)),
    );
    // Cursor Agent over the native SDK shim, same lazy pattern: the static
    // descriptor mirrors CursorHarness exactly. No mid-turn steering
    // extension (turn boundaries) and no effort ladder — Cursor bakes effort
    // into the model id's bracket suffix instead of a `thought_level` option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Cursor,
            name: "Cursor".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Cursor),
        },
        Box::new(|| komet_harness::CursorHarness::new().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::CursorHarness::new()) as Arc<dyn Harness>)),
    );
    // Grok Build over ACP, same lazy pattern: the static descriptor mirrors
    // AcpHarness::grok() exactly. No `_session/steering` extension yet, so
    // steers deliver at turn boundaries; the effort ladder applies per
    // session via the `thought_level` config option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Grok,
            name: "Grok".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Grok),
        },
        Box::new(|| komet_harness::AcpHarness::grok().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::grok()) as Arc<dyn Harness>)),
    );
    // Hermes Agent over ACP (`hermes acp`), same lazy pattern: the static
    // descriptor mirrors AcpHarness::hermes() exactly. No steering extension
    // (turn boundaries) and no effort ladder — Hermes exposes no effort
    // config over ACP today (hybrid reasoning is model-internal).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Hermes,
            name: "Hermes".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Hermes),
        },
        Box::new(|| komet_harness::AcpHarness::hermes().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::hermes()) as Arc<dyn Harness>)),
    );
    // opencode over its native ACP server (`opencode acp`), same lazy
    // pattern: the static descriptor mirrors AcpHarness::opencode() exactly.
    // No steering extension and no thought_level, so steers deliver at turn
    // boundaries with an empty reasoning ladder.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Opencode,
            name: "OpenCode".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            // Always show OpenCode in the Agents list; the live CLI probe
            // determines whether it can be enabled or managed.
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Opencode),
        },
        Box::new(|| komet_harness::AcpHarness::opencode().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::opencode()) as Arc<dyn Harness>)),
    );
    // pi over ACP (community `pi-acp` adapter), same lazy pattern: the static
    // descriptor mirrors AcpHarness::pi() exactly — turn-boundary steering,
    // pi's thinking ladder minus its "off" tier.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Pi,
            name: "Pi".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Pi),
        },
        Box::new(|| komet_harness::AcpHarness::pi().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::pi()) as Arc<dyn Harness>)),
    );
    // Google Antigravity CLI (`agy`) uses its documented headless NDJSON
    // protocol; it is discovered lazily like the other local agents.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Antigravity,
            name: "Antigravity".into(),
            supports_steering: false,
            steering_mode: SteeringMode::TurnBoundary,
            // Effort is per-model (baked into Gemini/GPT ids; unsupported on Claude).
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Antigravity),
        },
        Box::new(|| komet_harness::AntigravityHarness::new().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AntigravityHarness::new()) as Arc<dyn Harness>)),
    );
    // Cline over its native ACP server (`cline --acp`), same lazy pattern:
    // turn-boundary steering and no `thought_level` config option, so the
    // reasoning ladder stays empty. `installed` stays a real CLI probe: Cline
    // requires the npm-installed `cline` binary (npm install -g cline) and a
    // one-time sign-in (`cline` in a terminal); the ACP agent IS the package,
    // so there is no opencode-style always-show row.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Cline,
            name: "Cline".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Cline),
        },
        Box::new(|| komet_harness::AcpHarness::cline().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::cline()) as Arc<dyn Harness>)),
    );
    // Qoder CLI over its native ACP server (`qoder --acp`), same lazy
    // pattern: turn-boundary steering and no advertised `thought_level`.
    // `installed` is a real CLI probe — Qoder needs its own login
    // (`qoder login` or QODER_PERSONAL_ACCESS_TOKEN).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Qoder,
            name: "Qoder".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Qoder),
        },
        Box::new(|| komet_harness::AcpHarness::qoder().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::qoder()) as Arc<dyn Harness>)),
    );
    // Devin CLI over its native ACP server (`devin acp`).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Devin,
            name: "Devin".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Devin),
        },
        Box::new(|| komet_harness::AcpHarness::devin().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::devin()) as Arc<dyn Harness>)),
    );
    // GitHub Copilot CLI over its native ACP server (`copilot --acp`).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Copilot,
            name: "GitHub Copilot".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Copilot),
        },
        Box::new(|| komet_harness::AcpHarness::copilot().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::AcpHarness::copilot()) as Arc<dyn Harness>)),
    );
    // Muse Code's native Muse Session Protocol driver (`muse serve`).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Muse,
            name: "Muse Code".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
            surface: komet_harness::capabilities::interaction_surface(HarnessId::Muse),
        },
        Box::new(|| komet_harness::MuseHarness::new().installed()),
        Box::new(|| Ok(Arc::new(komet_harness::MuseHarness::new()) as Arc<dyn Harness>)),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_slot_lists_without_resolving() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Lazy Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
                surface: komet_harness::capabilities::interaction_surface(HarnessId::Mock),
            },
            Box::new(|| false),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        // The listing runs the probe, not the stored placeholder.
        assert!(!listed[0].installed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn catalog_surfaces_are_the_harness_table_projected_not_recopied() {
        // Task 0's single source of truth: `ListHarnesses` carries the surface
        // the harness crate declares, so a descriptor that drifted from the
        // table (or a lazy slot that forgot it) fails here.
        let registry = default_registry();
        let descriptors = registry.descriptors();
        assert_eq!(descriptors.len(), 14);
        for descriptor in &descriptors {
            assert_eq!(
                descriptor.surface,
                komet_harness::capabilities::interaction_surface(descriptor.id),
                "{}'s catalog surface differs from the harness table",
                descriptor.id.as_str()
            );
            assert_ne!(
                descriptor.surface,
                komet_proto::HarnessInteractionSurface::default(),
                "an all-absent surface means the projection did not run"
            );
        }
    }

    #[test]
    fn default_registry_lists_all_supported_agent_slots() {
        let registry = default_registry();
        let ids: Vec<HarnessId> = registry.descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                HarnessId::Mock,
                HarnessId::ClaudeCode,
                HarnessId::Codex,
                HarnessId::Cursor,
                HarnessId::Grok,
                HarnessId::Hermes,
                HarnessId::Opencode,
                HarnessId::Pi,
                HarnessId::Antigravity,
                HarnessId::Cline,
                HarnessId::Qoder,
                HarnessId::Devin,
                HarnessId::Copilot,
                HarnessId::Muse,
            ]
        );
        assert!(registry.resolve(HarnessId::Mock).is_ok());
        assert!(registry.resolve(HarnessId::ClaudeCode).is_ok());
        // A codex-configured chat resolves the right harness (construction is
        // cheap; CLI discovery is deferred to models()/run()).
        let codex = registry.resolve(HarnessId::Codex).unwrap();
        assert_eq!(codex.id(), HarnessId::Codex);
        // Grok resolves through the shared ACP harness; its descriptor must
        // mirror the resolved harness (descriptor-stability rule).
        let grok = registry.resolve(HarnessId::Grok).unwrap();
        assert_eq!(grok.id(), HarnessId::Grok);
        assert_eq!(grok.display_name(), "Grok");
        assert_eq!(grok.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            grok.reasoning_levels(),
            &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
        // Cursor, Hermes and Pi mirror their specs the same way.
        let cursor = registry.resolve(HarnessId::Cursor).unwrap();
        assert_eq!(cursor.id(), HarnessId::Cursor);
        assert_eq!(cursor.display_name(), "Cursor");
        assert_eq!(cursor.steering_mode(), SteeringMode::TurnBoundary);
        assert!(cursor.reasoning_levels().is_empty());
        let hermes = registry.resolve(HarnessId::Hermes).unwrap();
        assert_eq!(hermes.id(), HarnessId::Hermes);
        assert_eq!(hermes.display_name(), "Hermes");
        assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
        assert!(hermes.reasoning_levels().is_empty());
        let opencode = registry.resolve(HarnessId::Opencode).unwrap();
        assert_eq!(opencode.id(), HarnessId::Opencode);
        assert_eq!(opencode.display_name(), "OpenCode");
        assert_eq!(opencode.steering_mode(), SteeringMode::TurnBoundary);
        // Effort rides model variants on the wire; the picker must not invent
        // a global Low/Medium/High ladder for variant-less Zen models.
        assert!(opencode.reasoning_levels().is_empty());
        let pi = registry.resolve(HarnessId::Pi).unwrap();
        assert_eq!(pi.id(), HarnessId::Pi);
        assert_eq!(pi.display_name(), "Pi");
        assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            pi.reasoning_levels(),
            &[
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max
            ]
        );
        let antigravity = registry.resolve(HarnessId::Antigravity).unwrap();
        assert_eq!(antigravity.id(), HarnessId::Antigravity);
        assert_eq!(antigravity.display_name(), "Antigravity CLI");
        assert_eq!(antigravity.steering_mode(), SteeringMode::TurnBoundary);
        // Effort is per-model (baked into the id, or unsupported on Claude).
        assert!(antigravity.reasoning_levels().is_empty());
        // Cline mirrors its ACP spec the same way: native `cline --acp`,
        // turn-boundary steers, no advertised thought_level.
        let cline = registry.resolve(HarnessId::Cline).unwrap();
        assert_eq!(cline.id(), HarnessId::Cline);
        assert_eq!(cline.display_name(), "Cline");
        assert_eq!(cline.steering_mode(), SteeringMode::TurnBoundary);
        // Cline's `--thinking` CLI tiers have no ACP `thought_level`
        // equivalent (verified against session/new): the picker must not
        // invent a ladder.
        assert!(cline.reasoning_levels().is_empty());
        let cline_desc = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Cline)
            .expect("cline descriptor");
        assert!(cline_desc.reasoning_levels.is_empty());
        let qoder = registry.resolve(HarnessId::Qoder).unwrap();
        assert_eq!(qoder.id(), HarnessId::Qoder);
        assert_eq!(qoder.display_name(), "Qoder");
        assert_eq!(qoder.steering_mode(), SteeringMode::TurnBoundary);
        assert!(qoder.reasoning_levels().is_empty());
        let devin = registry.resolve(HarnessId::Devin).unwrap();
        assert_eq!(devin.id(), HarnessId::Devin);
        assert_eq!(devin.display_name(), "Devin");
        assert_eq!(devin.steering_mode(), SteeringMode::TurnBoundary);
        assert!(devin.reasoning_levels().is_empty());
        let copilot = registry.resolve(HarnessId::Copilot).unwrap();
        assert_eq!(copilot.id(), HarnessId::Copilot);
        assert_eq!(copilot.display_name(), "GitHub Copilot");
        assert_eq!(copilot.steering_mode(), SteeringMode::TurnBoundary);
        assert!(copilot.reasoning_levels().is_empty());
        let muse = registry.resolve(HarnessId::Muse).unwrap();
        assert_eq!(muse.id(), HarnessId::Muse);
        assert_eq!(muse.display_name(), "Muse Code");
        assert_eq!(muse.steering_mode(), SteeringMode::StepBoundary);
        assert!(muse.supports_steering());
        assert!(muse.reasoning_levels().is_empty());
        let agy_desc = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Antigravity)
            .expect("antigravity descriptor");
        assert!(agy_desc.reasoning_levels.is_empty());
    }

    /// Catalogs serialized by engines that predate the `installed`/`enabled`
    /// fields must keep deserializing — installed, and enabled per the
    /// default-set fallback (Claude Code yes, Grok no).
    #[test]
    fn descriptor_without_new_fields_parses_with_fallbacks() {
        let parse = |id: &str| -> HarnessDescriptor {
            serde_json::from_str(&format!(
                r#"{{
                    "id": "{id}",
                    "name": "x",
                    "supportsSteering": true,
                    "steeringMode": "step-boundary",
                    "reasoningLevels": []
                }}"#
            ))
            .unwrap()
        };
        let claude = parse("claude-code");
        assert!(claude.installed);
        assert_eq!(claude.enabled, None);
        assert!(descriptor_enabled(&claude));
        assert!(!descriptor_enabled(&parse("grok")));
    }

    /// A registry slot for the tests below: installed probe fixed, factory
    /// never expected to run.
    fn test_slot(registry: &HarnessRegistry, id: HarnessId, installed: bool) {
        registry.register_lazy(
            HarnessDescriptor {
                id,
                name: format!("{id:?}"),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
                surface: komet_harness::capabilities::interaction_surface(id),
            },
            Box::new(move || installed),
            Box::new(|| Err(HarnessError::NotInstalled("test slot".into()))),
        );
    }

    /// `descriptors()` stamps the per-device enabled flag; `set_enabled`
    /// guards the gate (no enabling missing CLIs, no disabling the last one)
    /// and persists across a reload.
    #[test]
    fn enablement_stamps_guards_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        test_slot(&registry, HarnessId::ClaudeCode, true);
        test_slot(&registry, HarnessId::Codex, true);
        test_slot(&registry, HarnessId::Grok, true);
        test_slot(&registry, HarnessId::Hermes, false);

        // Default set stamped: Claude Code + Codex on, the rest off.
        let flags: Vec<(HarnessId, Option<bool>)> = registry
            .descriptors()
            .into_iter()
            .map(|d| (d.id, d.enabled))
            .collect();
        assert_eq!(
            flags,
            vec![
                (HarnessId::ClaudeCode, Some(true)),
                (HarnessId::Codex, Some(true)),
                (HarnessId::Grok, Some(false)),
                (HarnessId::Hermes, Some(false)),
            ]
        );

        // The gate: a missing CLI can't be enabled; unknown ids refuse.
        assert!(registry.set_enabled(HarnessId::Hermes, true).is_err());
        assert!(registry.set_enabled(HarnessId::Pi, true).is_err());
        // Installed CLIs toggle both ways; no-op flips are fine.
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Codex, false).unwrap();
        registry.set_enabled(HarnessId::ClaudeCode, false).unwrap();
        // Grok is the last one standing — refusing keeps the composer usable.
        assert!(registry.set_enabled(HarnessId::Grok, false).is_err());
        assert_eq!(registry.enabled_set(), vec![HarnessId::Grok]);

        // A fresh registry over the same data dir reads the persisted set.
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        assert_eq!(reloaded.enabled_set(), vec![HarnessId::Grok]);
    }

    /// Discovered models persist to `model-cache.json` and serve a fresh
    /// registry from disk — the picker never re-probes an agent across app
    /// launches (opencode's cold Node boot measured ~13s under the app's
    /// 4-way boot prefetch; user report: "model loading is too slow").
    #[tokio::test]
    async fn models_cache_persists_and_reloads_from_disk() {
        use komet_harness::mock::MockHarness;
        let dir = tempfile::tempdir().unwrap();

        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        registry.load_models_cache(dir.path());
        registry.register(Arc::new(MockHarness { script: Vec::new() }));

        // A cache miss probes the harness and writes through.
        let models = registry.models(HarnessId::Mock, false).await.unwrap();
        assert!(!models.is_empty(), "{models:?}");
        assert!(
            dir.path().join("model-cache.json").exists(),
            "a successful probe must persist the list"
        );

        // A fresh registry over the same data dir serves the cached list
        // without re-probing (the harness was never registered here).
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        reloaded.load_models_cache(dir.path());
        let cached = reloaded.models(HarnessId::Mock, false).await;
        let ids: Vec<String> = cached.unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["mock-1", "mock-fable-5"]);
    }

    #[tokio::test]
    async fn startup_model_warm_replaces_a_fresh_disk_cache_with_live_models() {
        use komet_harness::mock::MockHarness;
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_models_cache(dir.path());
        registry.register(Arc::new(MockHarness { script: Vec::new() }));

        registry.models_cache_entries().insert(
            HarnessId::Mock,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: vec![Model {
                    id: "obsolete-model".into(),
                    label: "Obsolete".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );

        registry.warm_models_cache(vec![HarnessId::Mock]).await;

        let cache = registry.models_cache_entries();
        let refreshed = cache[&HarnessId::Mock]
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(refreshed, ["mock-1", "mock-fable-5"]);
    }

    /// A fresh cache entry serves instantly (age under the refresh window),
    /// while an empty wire result falls back to a stale cached list instead
    /// of returning nothing.
    #[tokio::test]
    async fn models_cache_serves_fresh_and_falls_back_on_empty() {
        use komet_harness::mock::MockHarness;
        let dir = tempfile::tempdir().unwrap();

        let registry = HarnessRegistry::new();
        registry.load_models_cache(dir.path());
        registry.register(Arc::new(MockHarness { script: Vec::new() }));
        let models = registry.models(HarnessId::Mock, false).await.unwrap();
        assert_eq!(models.len(), 2);

        // Fresh path: second call returns from the in-memory cache (no
        // re-probe observable, but the entry's age stays under the window).
        let again = registry.models(HarnessId::Mock, false).await.unwrap();
        assert_eq!(again.len(), 2);
        let entry = registry
            .models_cache_entries()
            .get(&HarnessId::Mock)
            .cloned()
            .unwrap();
        assert!(crate::now_ms() - entry.discovered_at_ms < 60_000);

        // Fallback: a wire-first harness whose probe advertises nothing
        // keeps the previously-cached list rather than emptying the picker.
        registry.models_cache_entries().insert(
            HarnessId::Opencode,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: vec![Model {
                    id: "opencode/smol".into(),
                    label: "Smol".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );
        // An empty wire answer (no catalog, no list) falls back to the cache.
        let fallback = {
            let harness = komet_harness::AcpHarness::opencode()
                .with_executable("/nonexistent/never-an-opencode-acp");
            // resolve via a lazy slot whose factory serves the harness
            // directly, so the fallback path is what decides.
            let reg = HarnessRegistry::new();
            reg.register(Arc::new(harness) as Arc<dyn Harness>);
            // Seed the same cache entry the parent holds.
            *reg.models_cache_entries() = registry.models_cache_entries().clone();
            reg.models(HarnessId::Opencode, false).await
        };
        assert_eq!(
            fallback.unwrap()[0].id,
            "opencode/smol",
            "a stale cached list must outlive an empty wire probe"
        );
    }

    #[tokio::test]
    async fn models_force_bypasses_cache_and_propagates_error() {
        let registry = HarnessRegistry::new();
        registry.load_models_cache(std::path::Path::new("/nonexistent-dir-xyz"));
        let err = registry
            .models(HarnessId::ClaudeCode, true)
            .await
            .expect_err("sans CLI ProbeError attendu");
        assert!(!err.to_string().is_empty());
    }

    /// `force=true` must never mask a probe failure with a stale cached
    /// list: the error propagates so the picker shows it, not old models.
    #[tokio::test]
    async fn models_force_never_serves_stale_on_probe_error() {
        let registry = HarnessRegistry::new();
        registry.load_models_cache(std::path::Path::new("/nonexistent-dir-xyz"));
        let harness = komet_harness::AcpHarness::opencode()
            .with_executable("/nonexistent/never-an-opencode-acp");
        registry.register(Arc::new(harness) as Arc<dyn Harness>);
        registry.models_cache_entries().insert(
            HarnessId::Opencode,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: vec![Model {
                    id: "opencode/smol".into(),
                    label: "Smol".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );
        let err = registry
            .models(HarnessId::Opencode, true)
            .await
            .expect_err("force=true must propagate the probe error, not stale cache");
        assert!(!err.to_string().is_empty());
    }

    /// The Codex lazy descriptor must be indistinguishable from `describe()`
    /// after the first resolve — otherwise the catalog entry silently changes
    /// the moment the harness is used (name/ladder flip in the picker rail).
    /// (KNOWN GAP, predates this slot: the claude-code descriptor advertises
    /// `[Ultrathink]` while the resolved adapter reports `[Low..Max]` — left
    /// as-is here; flagged for its own pass.)
    #[test]
    fn codex_lazy_descriptor_matches_resolved_harness() {
        let registry = default_registry();
        let before = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        registry.resolve(HarnessId::Codex).unwrap();
        let after = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        assert_eq!(before.name, after.name);
        assert_eq!(before.supports_steering, after.supports_steering);
        assert_eq!(before.steering_mode, after.steering_mode);
        assert_eq!(before.reasoning_levels, after.reasoning_levels);
    }

    /// Cache-first: once a list is known, `models(force=false)` must serve it
    /// WITHOUT resolving the harness again — that is what stops a picker open
    /// or an agent switch from re-running a cold CLI probe. `force=true`
    /// (Rescan) still resolves and probes.
    #[tokio::test]
    async fn cached_models_are_served_without_resolving_the_harness() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let resolves = Arc::new(AtomicUsize::new(0));
        let counted = resolves.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
                surface: komet_harness::capabilities::interaction_surface(HarnessId::Mock),
            },
            Box::new(|| true),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(MockHarness { script: vec![] }) as Arc<dyn Harness>)
            }),
        );

        // Cold picker: one probe, which resolves the harness and caches.
        let first = registry
            .models(HarnessId::Mock, false)
            .await
            .expect("first load probes");
        assert!(!first.is_empty());
        assert_eq!(resolves.load(Ordering::SeqCst), 1);

        // Overwrite the cache with a marker list so the next answer's origin
        // is unambiguous: the marker means "cache", anything else "probe".
        registry.models_cache_entries().insert(
            HarnessId::Mock,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: vec![Model {
                    id: "from-cache".into(),
                    label: "From cache".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );

        let cached = registry
            .models(HarnessId::Mock, false)
            .await
            .expect("cached load");
        assert_eq!(
            cached[0].id, "from-cache",
            "a known list must be served from the cache, not re-probed"
        );
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "a cache hit must not resolve (and therefore not probe) the harness"
        );

        // Rescan / Retry: bypasses the cache and probes the CLI.
        let forced = registry
            .models(HarnessId::Mock, true)
            .await
            .expect("forced probe");
        assert_ne!(
            forced[0].id, "from-cache",
            "force=true must probe the harness instead of serving the cache"
        );
    }

    /// The boot grace must suppress stale-entry background refreshes: the UI
    /// prefetches models while the window is still initializing, and probing
    /// there would starve GPU/window init with agent CLI spawns. Once the
    /// grace has passed, the same call revalidates in the background.
    #[tokio::test]
    async fn stale_cache_refresh_waits_for_boot_grace() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut registry = HarnessRegistry::new();
        let resolves = Arc::new(AtomicUsize::new(0));
        let counted = resolves.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
                surface: komet_harness::capabilities::interaction_surface(HarnessId::Mock),
            },
            Box::new(|| true),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(
                    Arc::new(komet_harness::mock::MockHarness { script: vec![] })
                        as Arc<dyn Harness>,
                )
            }),
        );
        registry.models_cache_entries().insert(
            HarnessId::Mock,
            CachedModels {
                discovered_at_ms: crate::now_ms()
                    - MODELS_CACHE_REFRESH_AFTER.as_millis() as i64
                    - 1,
                models: vec![Model {
                    id: "stale".into(),
                    label: "Stale".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );

        let served = registry.models(HarnessId::Mock, false).await.unwrap();
        assert_eq!(served[0].id, "stale", "a stale entry is still served");
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            0,
            "during the boot grace the stale entry must not be re-probed"
        );

        registry.booted_at =
            std::time::Instant::now() - BOOT_MODEL_GRACE - std::time::Duration::from_secs(1);
        let served = registry.models(HarnessId::Mock, false).await.unwrap();
        assert_eq!(
            served[0].id, "stale",
            "stale rows stay visible while refreshing"
        );
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "past the grace the stale entry revalidates in the background"
        );
    }

    /// A crashing CLI with nothing cached shows the harness's curated catalog
    /// instead of a dead-end error, while the background retry keeps running.
    #[tokio::test]
    async fn failed_probe_without_cache_serves_the_curated_fallback() {
        let registry = HarnessRegistry::new();
        let harness = komet_harness::claude::ClaudeHarness::new()
            .with_executable("/nonexistent/never-a-claude");
        registry.register(Arc::new(harness) as Arc<dyn Harness>);

        let models = registry
            .models(HarnessId::ClaudeCode, false)
            .await
            .expect("a curated fallback must outlive a failed first probe");
        assert_eq!(
            models.len(),
            komet_harness::claude::catalog::static_models().len(),
            "the fallback is the harness's curated catalog"
        );
        assert!(!models.is_empty());
    }
}
