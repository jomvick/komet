# Settings › Usage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A local-first Settings › Usage page combining provider quota windows with Komet-measured token usage, scoped to milestones M0–M2 of issue #53.

**Architecture:** Engine gains `usage.rs`, a local JSONL store (`{store_root}/usage/{turns,quota}.jsonl`, 180-day retention, atomic rewrites). Turn records are appended from the run pipeline on `AgentEvent::Usage` (harness/model from the live session, project = session cwd); quota samples are appended by `AgentAccounts` after each successful probe. A single `GetUsageReport` RPC joins the active account's quota windows with aggregates. The UI page loads it on open, never polls while hidden, and does manual Refresh through the existing 30 s cooldown.

**Tech Stack:** Rust 2024, gpui, serde, chrono. No new dependencies.

**Spec:** GitHub issue jomvick/komet#53 (M0–M2); `docs/superpowers/specs/2026-10-30-settings-providers-design.md` for page conventions.

## Global Constraints

- Usage index and quota history stay local and unsynced; never copy message text, prompts, credentials, or provider tokens.
- Missing provider data renders as "Not reported", never as `0` or `0%`.
- Every figure is labelled Provider or Komet-measured.
- Retention: 180 days, bounded; no background polling while the page is hidden.
- No new dependencies; no charts library.

## Review Focus

- A model that never reports `limit_window_seconds` (any provider) must not fabricate pace or time-to-exhaustion.
- A turn before the first `SessionStarted` (or with no model) must still be counted, with model `None`.
- An empty/absent quota history must render the account card without a fake "0% used".
- A `targetDeviceId` for a remote workspace must be honoured on every Usage RPC.
- Torn/garbage JSONL lines (crash mid-append) must be skipped, not fail the page.

---

### Task 1: Proto — window duration + report types

**Files:**
- Modify: `crates/proto/src/entities.rs` (`AgentUsageWindow`, new report types)
- Test: same file `#[cfg(test)]` or existing proto tests

**Interfaces:**
- Produces: `AgentUsageWindow.window_seconds: Option<u32>` (serde camelCase, skip when None);
  `UsageTotals { runs, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens }`;
  `AgentUsageReport { harness: HarnessId, account: Option<AgentAccount>, last_quota_at: Option<DateTime<Utc>>, last_day: UsageTotals, last_30_days: UsageTotals }`;
  `UsageReport { agents: Vec<AgentUsageReport> }`.

- [ ] Step 1: add the field/types, `#[serde(default)]` on `window_seconds`.
- [ ] Step 2: `cargo check -p komet-proto`.

### Task 2: Engine store `crates/engine/src/usage.rs`

**Files:** Create `crates/engine/src/usage.rs`; re-export in `crates/engine/src/lib.rs`.

**Interfaces:**
- `pub struct UsageStore` with `open(root: &Path) -> io::Result<Self>`, `record_turn(&self, &TurnUsage) -> io::Result<()>`, `record_quota(&self, &QuotaSample) -> io::Result<()>`, `totals(&self, since: DateTime<Utc>) -> Vec<(HarnessId, UsageTotals)>`, `last_quota_at(&self) -> Vec<(HarnessId, String, DateTime<Utc>)>`.
- `TurnUsage { ts: i64, chat: String, harness: HarnessId, model: Option<String>, project: Option<String>, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens }`.
- `QuotaSample { ts: i64, device: String, harness: HarnessId, account: String, windows: Vec<AgentUsageWindow> }`.
- `pub const RETENTION_DAYS: i64 = 180;`.

- [ ] Step 1: tests first — append/read round-trip; retention drops old lines; malformed line skipped; aggregation sums two windows and ignores other agents; empty file → empty totals.
- [ ] Step 2: implement (JSONL append, `prune_on_open` filtering `ts >= now - RETENTION_DAYS`, temp+rename rewrite).
- [ ] Step 3: `cargo test -p komet-engine usage::`.

### Task 3: Engine wiring — turn ingestion, quota samples, RPC

**Files:**
- Modify: `crates/engine/src/lib.rs` (field + `EngineRpc::new`), `crates/engine/src/rpc.rs`, `crates/rpc/src/lib.rs` (method const + forwardable), `crates/engine/src/sessions.rs`, `crates/engine/src/agent_accounts.rs`.

**Interfaces:**
- `SessionsEngine::set_usage_store(&self, Arc<UsageStore>)`; ingestion inside `publish` on `AgentEvent::Usage` uses `harness_for(chat_id)`, last `SessionStarted.model`, and `journal_harness_session(chat_id).cwd`.
- `AgentAccounts::set_usage_store(&self, Arc<UsageStore>)`; `usage_for` records a `QuotaSample` on success (account = slot email, device = engine device id).
- `AgentAccounts::list_only(&self, mode: UsageMode, only: &[HarnessId])`; `list` delegates with `&[]` meaning all.
- Fresh-probe cooldown: 30 s per `harness:account_key`; inside the cooldown a `Fresh` request serves the cache.
- `methods::GET_USAGE_REPORT = "GetUsageReport"` (forwardable); params `{ agents?: [HarnessId], usageMode?: String }`; reply `UsageReport`.

- [ ] Step 1: engine test asserting a `Usage` event journaled through `publish` yields a turn record with harness and model.
- [ ] Step 2: engine test asserting a fresh quota probe writes a sample (mock server or direct `record_quota` path).
- [ ] Step 3: implement + wire; add `GET_USAGE_REPORT` to `forwardable()`.
- [ ] Step 4: `cargo test -p komet-engine usage`.

### Task 4: Provider window durations

**Files:** Modify `crates/engine/src/agent_accounts.rs` parsers + tests.
- Claude: Session `18_000` s, Week `604_800` s.
- Codex: `limit_window_seconds` (>0).
- Cursor: `requestWindowSeconds` (>0); monthly fallbacks `None`.
- Antigravity: bucket `window` (`daily|weekly|monthly`) → seconds.
- Copilot: `None` (calendar month).

- [ ] Step 1: extend parser tests to assert `window_seconds`.
- [ ] Step 2: populate, run `cargo test -p komet-engine agent_accounts`.

### Task 5: Settings destination + Usage page

**Files:**
- Modify: `crates/ui/src/shell/navigation.rs` (`SettingsSection::Usage`, `ALL` 11 entries, label, `nav_icon` = `icons::SORT_VERTICAL`), `crates/ui/src/shell.rs` (field + import), `crates/ui/src/settings.rs` (`pub mod usage;`, `UiSettings.usage_agents: Vec<String>` default `["claude-code","codex"]`).
- Create: `crates/ui/src/settings/usage.rs`.
- Test: `crates/ui/src/shell/tests.rs` count 10 → 11.

**Interfaces:**
- `UsagePage::new(state: Entity<AppState>, selected: Vec<HarnessId>, cx)`; `UsagePage::on_shown(cx)` marks visible + reloads with `usageMode: "cached"`; `on_hidden()` clears visibility. Emits `UsageEvent::SelectionChanged(Vec<HarnessId>)` so the shell persists `usage_agents`.
- RPC params always run through `with_target`.

- [ ] Step 1: destination enum + shell wiring + count test.
- [ ] Step 2: page loads `GetUsageReport` on show, renders header, `+` agent menu (capability labels), removable chips.
- [ ] Step 3: `cargo check -p komet-ui`.

### Task 6: Agent section — quota, pace, measured totals, refresh

**Files:** `crates/ui/src/settings/usage.rs` (+ pure fns + tests).

**Interfaces:**
- `pub fn pace(window: &AgentUsageWindow, now: DateTime<Utc>) -> Option<Pace>` with `Pace { elapsed_fraction: f32, delta: f32 }`; requires `window_seconds > 0`, `resets_at` present, `now < resets_at`.
- `pub fn time_to_exhaustion(window: &AgentUsageWindow, previous: Option<&(DateTime<Utc>, f32)>, now: DateTime<Utc>) -> Option<f64>` (hours); `None` under 10 min of history, non-increasing usage, or `used_fraction >= 1.0`.
- Render: provider windows (label, %, reset countdown, source badge), pace line (`+N pts ahead` / `N pts behind` / `On pace`), estimate when available, "Not reported" when absent, measured block with 24 h / 30 d tokens + runs and a "Komet-measured" label, Refresh button with 30 s cooldown disabled state.

- [ ] Step 1: pure-function tests — ahead/behind, reset passed, missing duration, missing resets, exhaustion projection valid/invalid.
- [ ] Step 2: implement render.
- [ ] Step 3: `cargo test -p komet-ui usage`.

### Task 7: Verification

- [ ] `cargo fmt --all`
- [ ] `cargo check --workspace --all-targets`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test -p komet-proto -p komet-engine -p komet-ui`
