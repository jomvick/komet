# Check Updates Devices Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a manual "Check for updates" section in Settings → Devices with explicit status text and spinner, wired to a new engine `CheckUpdate` RPC that wakes the existing release checker; when an update exists the existing update modal/strip opens automatically.

**Architecture:** New `CheckUpdate` RPC calls `Updater::check_now()` (existing wake channel); DevicesPage calls it and renders checking/up-to-date/error states from `AppState.update`; Shell's existing `on_state_changed → newly_detected_update → update_modal_version` flow opens the popup with no changes.

**Tech Stack:** Rust, gpui (UI), tokio, existing `komet-update`, `komet-rpc`, `komet-engine`.

**Spec:** Chat agreement 2026-10-12 — explicit component (not icon-only) in Devices settings; button "Check for updates" with refresh icon; animation while checking; if update → trigger existing popup; if none → "You're up to date" message with version.

## Global Constraints

- Follow existing gpui patterns in `crates/ui/src/settings/devices.rs` and `crates/ui/src/settings/widgets.rs` (section_card, ghost/primary buttons).
- RPC method name is `CheckUpdate` (PascalCase like `UpdateStatus`/`ApplyUpdate`).
- `CheckUpdate` is relay-forwardable like `UpdateStatus`/`ApplyUpdate` (device-local check).
- Never bypass manifest signature/sha256 verification; manual check only wakes the checker, it does not download.
- Copy stays explicit: button label "Check for updates", checking label "Checking…", up-to-date message includes version.

## Review Focus

- Manual re-check of an already-observed version does not reopen the modal (Shell dedupes by `update_observed`) — inline "Update available" row must still show, which a reasonable user expects.
- Offline engine: `CheckUpdate` wakes the loop but fetch fails; DevicesPage must show the engine error text rather than spin forever, which a reasonable user expects.
- Rapid double-click on Check must not spawn parallel checks/tasks, which a reasonable user expects.
- Unmanaged installs (source builds) get the same Check button but `ApplyUpdate` stays report-only; the section must not promise auto-install there, which a reasonable user expects.
- Screen-reader/keyboard user tabs to the Check button and activates it; focus and enabled state must remain sane while checking, which a reasonable user expects.

---

### Task 1: Engine CheckUpdate RPC

**Files:**
- Modify: `crates/rpc/src/lib.rs` (add method constant)
- Modify: `crates/engine/src/rpc.rs` (dispatch + forwardable list + stream list unchanged)
- Test: `cargo test -p komet-engine` (existing suite must pass; add dispatch cover if harness allows)

**Interfaces:**
- Consumes: `komet_update::Updater::check_now(&self)` (`crates/update/src/lib.rs:681`, sync, idempotent wake via `check_tx.send_modify`).
- Produces: `methods::CHECK_UPDATE: &str = "CheckUpdate"`; engine dispatch `methods::CHECK_UPDATE => { self.updater()?.check_now(); RpcReply::value(ok:true) }`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn check_update_method_is_forwardable() {
    assert!(is_forwardable("CheckUpdate"));
}
```

Note: place alongside existing rpc method tests if present; otherwise assert via the same `is_forwardable` fn used at `crates/engine/src/rpc.rs:1074`. If no unit harness exists there, write the constant assertion in `crates/rpc` tests.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-rpc check_update -- --nocapture`
Expected: FAIL (constant / branch missing).

- [ ] **Step 3: Implement `CHECK_UPDATE` in `crates/rpc/src/lib.rs`**

Add next to `UPDATE_STATUS`/`APPLY_UPDATE`:
```rust
pub const CHECK_UPDATE: &str = "CheckUpdate";
```
One line, same doc style (wakes the release checker immediately).

- [ ] **Step 4: Implement dispatch in `crates/engine/src/rpc.rs`**

Add `methods::CHECK_UPDATE` to the relay-forwardable match (~line 1074) and a dispatch arm next to `UPDATE_STATUS` (~line 1507):
```rust
methods::CHECK_UPDATE => {
    self.updater()?.check_now();
    RpcReply::value(&serde_json::json!({ "ok": true }))
}
```
Do NOT add to `is_stream_method` (one-shot, not a stream).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p komet-rpc -p komet-engine`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/rpc/src/lib.rs crates/engine/src/rpc.rs
git commit -m "feat: add CheckUpdate RPC to wake release checker"
```

### Task 2: DevicesPage Updates section UI

**Files:**
- Modify: `crates/ui/src/settings/devices.rs` (new Updates card + state)
- Test: `cargo test -p komet-ui devices` (existing presence/subtitle tests must pass; add pure-helper test below)

**Interfaces:**
- Consumes: `methods::CHECK_UPDATE` (Task 1); `AppState::update: Option<komet_update::UpdateStatus>` (`crates/ui/src/state.rs:629`); `komet_update::current_version()`; `crate::icons::REFRESH`; `crate::loaders::mini_gradient_spinner` for the checking animation; `widgets::section_card`, `widgets::field_label`.
- Produces: DevicesPage renders Updates card; while `checking==true` button disabled with spinner + "Checking…"; after, inline status text. Shell auto-opens existing `update-modal` on new `UpdateStatus` (no change needed in `shell.rs`).

- [ ] **Step 1: Write the failing test for the status copy helper**

```rust
#[test]
fn update_section_copy_states_current_version() {
    assert_eq!(update_check_summary(None, None), "Never checked for updates");
    assert!(update_check_summary(Some(42), Some(false)).contains("up to date"));
}
```

Helper signature to create in `devices.rs`:
```rust
fn update_check_summary(checked_at_ms: Option<i64>, update_available: Option<bool>) -> String
```
Rules: `None` → "Never checked for updates"; `Some(true)` → "Update available"; `Some(false)` → "You're up to date". Keep pure for testability; version + time-ago formatting stays in render.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-ui update_check_summary`
Expected: FAIL (helper not defined).

- [ ] **Step 3: Implement helper `update_check_summary` in `crates/ui/src/settings/devices.rs`**

Exact signature above, pure, no cx. Three branches only.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-ui update_check_summary`
Expected: PASS.

- [ ] **Step 5: Implement Updates card state + render in `DevicesPage`**

State additions on `DevicesPage`: `checking: bool`, `check_error: Option<SharedString>`, `check_task: Option<Task<()>>`, `just_checked: bool`.
Method `check_updates(cx)`:
- guard `if self.checking { return; }`; set `checking=true`, `check_error=None`, `cx.notify()`.
- clone engine handle; `cx.spawn`: `engine.client().call(methods::CHECK_UPDATE, serde_json::json!({})).await`; on completion set `checking=false`, `just_checked=true`, on `Err` set `check_error`; `cx.notify()`.
- Result text itself comes from `self.state.read(cx).update` on next render (stream will refresh it); do NOT snapshot version in the task.

Render (below devices card, same `page_column`): `widgets::section_card` containing:
- `field_label("Updates")` + current version line `Running v{current_version()}`.
- status line via `update_check_summary(status.checked_at, status.update_available)` + time-ago when available; error strip via `widgets::error_strip` when `check_error` or `status.error` present.
- Button: `widgets::ghost_action` or primary with `.id("updates-check")`, icon `crate::icons::REFRESH` size 14 + label "Check for updates" / "Checking…" when checking; `.when(checking, disabled + spinner via mini_gradient_spinner)`; `on_click` calls `check_updates`.
- When `status.update_available==true`, show inline "Update available — v{latest}" row (modal opens via Shell automatically for new versions; inline row covers the already-observed/dismissed case).

- [ ] **Step 6: Run UI tests + fmt**

Run: `cargo test -p komet-ui devices && cargo fmt --check`
Expected: PASS, no diff (run `cargo fmt` if needed).

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/settings/devices.rs
git commit -m "feat: manual check-for-updates section in Devices settings"
```
