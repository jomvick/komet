//! Settings → Agents / accounts (feature-inventory §1.9): provider cards
//! (Claude Code, Codex) with account rows — email, plan badge, Active, usage
//! meters (indigo → amber ≥80% → red ≥95%, reset time), Switch / Forget — plus
//! the add-account dialogs (paste-code and browser-poll flows) and
//! account-shaped loading skeletons. Komet retargets devices from the settings
//! sidebar (`targetDeviceId` passthrough kept plumbed, unused single-device).
//!
//! The accounts RPC surface is being implemented engine-side in parallel —
//! every call here surfaces failures as inline UI states rather than assuming
//! the methods exist.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, Hsla, ScrollHandle, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use std::time::Duration;

use komet_engine::UsageMode;
use komet_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, AgentUsageWindow, HarnessId,
};
use komet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Why a `ListAgentAccounts` load is happening. Pure input to
/// [`usage_mode_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    /// Page construction — the visit's first list.
    Mount,
    /// "Click to retry" after a failed load — still the visit's first
    /// successful list.
    Retry,
    /// The explicit Refresh button.
    Refresh,
    /// After a completed add-account login flow.
    PostLogin,
    /// After Switch/Forget succeeds.
    PostAction,
}

/// How aggressively a load should resolve usage, sent to the engine as
/// `usageMode`. Replaces the old `forceUsage` boolean, which could not express
/// the distinction that mattered: a forced list still served the 60s cache, so
/// a Refresh inside the TTL returned the previous numbers without a probe.
///
/// - `Cached` — the visit's first list (mount, or retry after a failure), and
///   post-Switch/Forget lists. A cold cache probes, so a first open never
///   renders "Usage unavailable"; a warm entry is still trustworthy, so we do
///   not stampede the provider on every nav. Post-Switch/Forget uses it because
///   the switch moved the LIVE login — the active account is probed through a
///   different path (live keyring / local language_server) than the saved-slot
///   copy, and that live entry was just invalidated by the switch itself.
/// - `Fresh` — the explicit Refresh button and a just-completed login. Both are
///   an explicit user request for current numbers, so the cache is bypassed
///   outright.
pub fn usage_mode_for(trigger: LoadTrigger) -> UsageMode {
    match trigger {
        LoadTrigger::Mount | LoadTrigger::Retry | LoadTrigger::PostAction => UsageMode::Cached,
        LoadTrigger::Refresh | LoadTrigger::PostLogin => UsageMode::Fresh,
    }
}

/// Compact absolute reset moment (komet settings.agents.tsx `formatReset`):
/// a local clock time ("3:45 PM") when it lands within ~22h,
/// a short weekday + time ("Sun 11:50 AM") within 7 days,
/// else month + day + time; the caller prefixes "resets ". Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    let diff = at.signed_duration_since(now);
    if diff.num_seconds() <= 0 {
        return Some("resets soon".into());
    }
    Some(if diff.num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else if diff.num_days() < 7 {
        format!(
            "resets {} {}",
            local.format("%a"),
            local.format("%-I:%M %p")
        )
    } else {
        format!(
            "resets {} {}",
            local.format("%b %-d"),
            local.format("%-I:%M %p")
        )
    })
}

/// Agents whose CLI login or provider credentials are required before runs.
/// The first four have native account management; the rest authenticate in
/// their own CLI and are listed here with the corresponding hand-off command.
pub const PROVIDERS: [(HarnessId, &str, &str); 13] = [
    (HarnessId::ClaudeCode, "Claude Code", "claude"),
    (HarnessId::Codex, "Codex", "codex"),
    (HarnessId::Antigravity, "Antigravity", "agy"),
    (HarnessId::Cursor, "Cursor", "cursor-agent"),
    (HarnessId::Devin, "Devin", "devin"),
    (HarnessId::Grok, "Grok", "grok"),
    (HarnessId::Hermes, "Hermes", "hermes"),
    (HarnessId::Opencode, "OpenCode", "opencode"),
    (HarnessId::Pi, "Pi", "pi"),
    (HarnessId::Muse, "Muse", "muse"),
    (HarnessId::Copilot, "GitHub Copilot", "copilot"),
    (HarnessId::Cline, "Cline", "cline"),
    (HarnessId::Qoder, "Qoder", "qoder"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountsEvent {
    FocusProvider(HarnessId),
}

impl EventEmitter<AccountsEvent> for AccountsPage {}

pub fn requires_login(harness: HarnessId) -> bool {
    PROVIDERS
        .iter()
        .any(|(provider, _, _)| *provider == harness)
}

pub fn cli_login_command(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::Devin => Some("devin auth login"),
        HarnessId::Grok => Some("grok"),
        HarnessId::Hermes => Some("hermes"),
        HarnessId::Opencode => Some("opencode auth login"),
        HarnessId::Pi => Some("pi"),
        HarnessId::Muse => Some("muse"),
        HarnessId::Copilot => None,
        HarnessId::Cline => Some("cline"),
        HarnessId::Qoder => Some("qoder login"),
        HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Cursor | HarnessId::Antigravity => {
            None
        }
        HarnessId::Mock => None,
    }
}

pub fn login_instructions(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Devin => {
            "Run `devin auth login` in a terminal and complete the browser sign-in."
        }
        HarnessId::Grok => "Run `grok` and sign in when prompted.",
        HarnessId::Hermes => "Run `hermes` and configure/sign in to a model provider.",
        HarnessId::Opencode => "Run `opencode auth login` and choose your model provider.",
        HarnessId::Pi => "Run `pi` and use `/login` to connect a model provider.",
        HarnessId::Muse => "Run `muse` and use `/login` to connect your account.",
        HarnessId::Copilot => "Use GitHub's device sign-in to connect and switch Copilot accounts.",
        HarnessId::Cline => "Run `cline` and choose a provider in its sign-in flow.",
        HarnessId::Qoder => "Run `qoder login` and complete the browser sign-in.",
        _ => "Use the Add account flow to sign in.",
    }
}
/// Accounts of one provider, active first (stable otherwise). Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    let mut accounts: Vec<&AgentAccount> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect();
    accounts.sort_by_key(|a| !a.active);
    accounts
}

/// Only the selected account's quota is meaningful for the live CLI session.
fn displayed_usage(account: &AgentAccount) -> Option<&[AgentUsageWindow]> {
    account.active.then_some(account.usage_windows.as_slice())
}

fn retain_previous_usage(
    snapshot: &mut AgentAccountsSnapshot,
    previous: Option<&AgentAccountsSnapshot>,
) {
    let Some(previous) = previous else { return };
    for account in &mut snapshot.accounts {
        if account.usage_windows.is_empty()
            && let Some(old) = previous
                .accounts
                .iter()
                .find(|old| old.id == account.id && !old.usage_windows.is_empty())
        {
            account.usage_windows = old.usage_windows.clone();
        }
    }
}

fn select_active_account(snapshot: &mut AgentAccountsSnapshot, harness: HarnessId, id: &str) {
    if !snapshot
        .accounts
        .iter()
        .any(|account| account.harness == harness && account.id == id)
    {
        return;
    }
    for account in &mut snapshot.accounts {
        if account.harness == harness {
            account.active = account.id == id;
        }
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

impl LoginFlow {
    /// Dialog title (komet: "Add Claude account" / "Add Codex account").
    fn title(&self) -> &'static str {
        let harness = match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. } => *harness,
        };
        match harness {
            HarnessId::Codex => "Add Codex account",
            HarnessId::Antigravity => "Add Antigravity account",
            HarnessId::Cursor => "Add Cursor account",
            HarnessId::Devin => "Add Devin account",
            HarnessId::Muse => "Add Muse account",
            HarnessId::Copilot => "Add Copilot account",
            _ => "Add Claude account",
        }
    }
}

/// `codex` binary lookup without a new dependency: scan `PATH` for an
/// executable `codex` file. Pure enough to unit-test via `PATH` manipulation.
fn codex_on_path() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("codex");
        if candidate.is_file() {
            return true;
        }
        #[cfg(windows)]
        if dir.join("codex.exe").is_file() {
            return true;
        }
    }
    false
}

pub struct AccountsPage {
    state: Entity<AppState>,
    /// Which device's logins are shown; `None` = this device (no passthrough).
    /// Retargeted by the page-header device switcher (komet parity: the
    /// accounts RPCs are relay-forwardable, CLI logins are per-device).
    target_device: Option<String>,
    device_menu: popover::Popup<()>,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// A `ListAgentAccounts` is in flight. Kept separate from the `Loadable`
    /// state so a refresh dims the button and leaves the cards (and their last
    /// known gauges) on screen instead of collapsing them to skeletons.
    refreshing: bool,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    focused_harness: Option<HarnessId>,
    scroll_target_pending: bool,
    copied_login_command: Option<HarnessId>,
    page_scroll: ScrollHandle,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    load_generation: u64,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let code_input = cx.new(|cx| ComposerInput::new("Paste the authorization code", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let target_device = state.read(cx).harness_target_device();
        let mut page = Self {
            state,
            target_device,
            device_menu: popover::Popup::default(),
            snapshot: Loadable::Idle,
            refreshing: false,
            busy_account: None,
            login: None,
            error: None,
            focused_harness: None,
            scroll_target_pending: false,
            copied_login_command: None,
            page_scroll: ScrollHandle::new(),
            code_input,
            load_task: None,
            load_generation: 0,
            action_task: None,
            poll_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        // Force the usage probe on the visit's first list — a plain list
        // returns no usage windows on a cold engine cache, which rendered
        // every account as "Usage unavailable" until a manual Refresh. The
        // Loading skeleton (meter ghosts) covers the probe latency, so
        // "Usage unavailable" is reserved for a probe that genuinely failed.
        page.load(usage_mode_for(LoadTrigger::Mount), cx);
        page
    }

    pub fn focus_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        self.focused_harness = Some(harness);
        self.scroll_target_pending = true;
        cx.notify();
    }

    fn copy_login_command(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if let Some(command) = cli_login_command(harness) {
            cx.write_to_clipboard(ClipboardItem::new_string(command.to_string()));
            self.copied_login_command = Some(harness);
            cx.notify();
        }
    }

    /// Retarget the page at another device's logins: every accounts RPC is
    /// relay-forwardable, so the whole page — list, usage probes, switch,
    /// forget, login flows — follows the passthrough.
    fn close_device_menu(&mut self, cx: &mut Context<Self>) {
        if self.device_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.device_menu);
            cx.notify();
        }
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.close_device_menu(cx);
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        // A different device = a different accounts world: drop in-flight
        // login/action state and reload with a forced usage probe (the new
        // device's cache is cold).
        self.login = None;
        self.busy_account = None;
        self.error = None;
        self.load(usage_mode_for(LoadTrigger::Mount), cx);
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// The page-header device switcher (komet device-switcher.tsx): a quiet
    /// trigger — platform glyph · name · presence dot · sort glyph — opening a
    /// dropdown of every registered device. Selecting one retargets the page.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        // Stable row order (registration time, then id) — komet's switcher
        // sorts the same way so rows never reshuffle on heartbeats.
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.success;
        let open = self.device_menu.is_open();

        let mut trigger =
            div()
                .id("accounts-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| this.device_menu.note_trigger_press()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    // A press that found the menu open closes it (the card's
                    // mouse-down-out already began the close) — never reopen.
                    if this.device_menu.take_press_was_open() {
                        this.close_device_menu(cx);
                    } else {
                        this.device_menu.open(());
                    }
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if self.device_menu.get().is_some() {
            let closing = self.device_menu.closing_since();
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_device_menu(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_local = is_local;
                    let pick_id = d.id.clone();
                    popover::menu_row(
                        theme,
                        is_active,
                        format!("accounts-device-row-{ix}"),
                        cx.entity_id(),
                    )
                    .id(("accounts-device-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Local device = no passthrough (calls stay direct).
                        let target = (!pick_local).then(|| pick_id.clone());
                        this.set_target_device(target, cx);
                    }))
                    .child(
                        icon(glyph)
                            .size(px(16.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .when(is_local, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted.opacity(0.35))
                                .child(SharedString::from("You")),
                        )
                    })
                    .child(
                        div()
                            .size(px(6.0))
                            .rounded_full()
                            .flex_none()
                            .bg(if is_local {
                                emerald
                            } else {
                                crate::theme::ink(0.2)
                            }),
                    )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu(
                "accounts-device-menu",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    fn load(&mut self, mode: UsageMode, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.refreshing = false;
            if matches!(self.snapshot, Loadable::Ready(_)) {
                self.error = Some("Usage refresh failed: engine not connected".into());
            } else {
                self.snapshot = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        // Only the first load has nothing to show yet; afterwards the previous
        // snapshot stays rendered while the new one is in flight (the Refresh
        // button dims via `refreshing`). Collapsing to `Loading` here was what
        // blanked every card to a skeleton and made the merge below dead code —
        // `snapshot.ready()` is always `None` mid-load.
        if matches!(self.snapshot, Loadable::Idle) {
            self.snapshot = Loadable::Loading;
        }
        self.refreshing = true;
        let params = self.params(serde_json::json!({ "usageMode": mode.as_wire() }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |page, cx| {
                if page.load_generation != generation {
                    return;
                }
                page.refreshing = false;
                let previous = page.snapshot.ready().cloned();
                let parsed = result.map_err(|err| err.to_string()).and_then(|value| {
                    serde_json::from_value::<AgentAccountsSnapshot>(value)
                        .map_err(|err| err.to_string())
                });
                page.snapshot = match parsed {
                    Ok(mut snapshot) => {
                        retain_previous_usage(&mut snapshot, previous.as_ref());
                        page.error = None;
                        Loadable::Ready(snapshot)
                    }
                    Err(message) if previous.is_some() => {
                        // Keep the last successful account and quota snapshot
                        // visible when a background refresh fails transiently.
                        page.error = Some(format!("Usage refresh failed: {message}").into());
                        Loadable::Ready(previous.expect("checked above"))
                    }
                    Err(message) => Loadable::Error(message),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Switch / Forget an account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            // Surface a dead engine handle instead of silently no-oping; the
            // headed log default is `warn`, so this stays visible there too.
            tracing::warn!(method, account = %account.id, "account_action dropped: no engine handle");
            self.error = Some("Engine is not running — reopen the Accounts page.".into());
            cx.notify();
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        // Tolerant param shape: both `id` and `accountId` plus the harness.
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        let account_id = account.id.clone();
        let account_harness = account.harness;
        let is_switch = method == methods::ACTIVATE_AGENT_ACCOUNT;
        tracing::debug!(method, account = %account.id, "account_action dispatching");
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            if let Err(err) = &result {
                // A failed switch/forget must be diagnosable under the headed
                // default filter (`warn`), not only with RUST_LOG=debug.
                tracing::warn!(method, error = %err, "account_action failed");
            }
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => {
                        if is_switch && let Loadable::Ready(snapshot) = &mut page.snapshot {
                            select_active_account(snapshot, account_harness, &account_id);
                        }
                        page.load(usage_mode_for(LoadTrigger::PostAction), cx);
                    }
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine is not running — reopen the Accounts page.".into());
            cx.notify();
            return;
        };
        self.login = Some(LoginFlow::Starting { harness });
        self.error = None;
        // Preflight hint only (never blocking): the page may target a REMOTE
        // device (`targetDeviceId` passthrough) while this check runs on the
        // local daemon — a missing local `codex` says nothing about the remote.
        if harness == HarnessId::Codex && !codex_on_path() {
            self.error = Some(
                "Local `codex` CLI not found on this device's PATH — if the target device lacks it too, run `codex login` there manually, then Refresh."
                    .into(),
            );
            // Fall through: the relayed RPC is the ground truth.
        }
        let params = self.params(serde_json::json!({ "harness": harness }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| komet_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        cx.open_url(&start.url);
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    harness,
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        let mut message = format!("Login failed to start: {err}");
                        if harness == HarnessId::Codex {
                            message.push_str(
                                " — fallback: run `codex login` in a terminal, then Refresh.",
                            );
                        }
                        page.error = Some(message.into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id, "code": code }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(usage_mode_for(LoadTrigger::PostLogin), cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The browser-wait poll loop: PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::Browser { start, .. }) = &self.login else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id }));
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let Some(LoginFlow::Browser { message, error, .. }) = &mut page.login else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(usage_mode_for(LoadTrigger::PostLogin), cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
                Some(start.login_id.clone())
            }
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).engine().cloned()) {
            let params = self.params(serde_json::json!({ "loginId": login_id }));
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    // ---- render pieces ----

    /// One usage window (komet settings.agents.tsx `UsageMeter`): label ·
    /// 5px rounded-full bar (indigo → amber ≥80% → red ≥95%) · "NN% used (MM% left)" ·
    /// reset time with prominence when exhausted.
    fn render_usage_meter(
        &self,
        window: &komet_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let remaining_fraction = (1.0 - fraction).clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme).opacity(match level {
            UsageLevel::Normal => 0.8,
            _ => 0.85,
        });
        let reset = format_reset(window.resets_at, now);
        let used_pct = (fraction * 100.0).round() as u32;
        let left_pct = (remaining_fraction * 100.0).round() as u32;
        let value = window.display_value.clone().unwrap_or_else(|| {
            if used_pct >= 100 {
                "100% used (0% left)".to_string()
            } else {
                format!("{}% used ({}% left)", used_pct, left_pct)
            }
        });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted.opacity(0.7))
            .child(
                div()
                    .w(px(72.0))
                    .flex_none()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(window.label.clone())),
            )
            .when(window.display_value.is_none(), |el| {
                el.child(
                    // Absolute track so `relative()` fill has a definite parent
                    // width (`flex_1` + content-sized fill used to collapse).
                    // Always paint the track: unused windows are 0% fill, and
                    // hiding that child made Gemini/Session look like "no gauge".
                    div()
                        .flex_1()
                        .min_w(px(56.0))
                        .max_w(px(220.0))
                        .h(px(5.0))
                        .relative()
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme.border)
                                .child(
                                    div()
                                        .h_full()
                                        // A 1.5% floor keeps tiny non-zero usage
                                        // visible (komet `max(used, 1.5)%`).
                                        .w(gpui::relative(if fraction > 0.0 {
                                            fraction.max(0.015)
                                        } else {
                                            0.0
                                        }))
                                        .rounded_full()
                                        .bg(fill),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .flex_none()
                    .text_right()
                    .child(SharedString::from(value)),
            )
            .when_some(reset, |el, reset| {
                el.child(
                    div()
                        .flex_none()
                        .truncate()
                        .text_color(if level == UsageLevel::Critical {
                            theme.danger_muted.opacity(0.9)
                        } else {
                            theme.text_muted.opacity(0.45)
                        })
                        .child(SharedString::from(reset)),
                )
            })
            .into_any_element()
    }

    /// One account row (komet settings.agents.tsx `AccountRow`): initial
    /// avatar, email + usage meters left; badges over the Switch/Forget
    /// actions right-anchored.
    ///
    /// The Switch/Forget element ids embed the account id, not the row index:
    /// gpui keys interactive element state by the id path, and row indices
    /// restart at zero per provider section — two buttons sharing
    /// `("account-switch", 0)` share one `pending_mouse_down` cell, and the
    /// first-painted one's capture-phase mouse-up clears it before the
    /// other's handler runs, silently eating every click on later sections.
    fn render_account_row(
        &self,
        account: &AgentAccount,
        first: bool,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let initial: SharedString = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let switch_account = account.clone();
        let forget_account = account.clone();
        // Globally-unique interactive ids — see render_account_row doc.
        let forget_id: SharedString = format!("account-forget-{}", account.id).into();
        let switch_id: SharedString = format!("account-switch-{}", account.id).into();

        let badges = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(account.active, |el| {
                el.child(widgets::badge_active(theme, "Active"))
            })
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            });

        // Actions only on INACTIVE accounts (komet `{!account.active && …}`):
        // an icon-only Forget (trash, hover → foreground) then Switch, which
        // reads "Switching…" while the activate round-trips.
        let actions: Option<gpui::Div> = (!account.active).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(forget_id)
                        .rounded(px(6.0))
                        .px(px(6.0))
                        .py(px(4.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .when(is_busy, |el| el.opacity(0.5))
                        .hover(|s| s.bg(crate::theme::ink(0.06)).text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(methods::FORGET_AGENT_ACCOUNT, &forget_account, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .when(account.switchable, |el| {
                    el.child(
                        crate::popover::btn_primary(
                            theme,
                            if is_busy { "Switching…" } else { "Switch" },
                        )
                        .id(switch_id)
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .text_size(px(11.5))
                        .when(is_busy, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(
                                methods::ACTIVATE_AGENT_ACCOUNT,
                                &switch_account,
                                cx,
                            );
                        })),
                    )
                })
        });

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                // Initial avatar: size-8 rounded-full border bg-white/[0.03].
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, email))
                    .map(|el| {
                        // Meters XOR the quiet fallback line — never both
                        // (komet: `usage ? meters : "Usage unavailable"…`).
                        if let Some(usage_windows) = displayed_usage(account) {
                            if usage_windows.is_empty() {
                                el.child(
                                    div()
                                        .mt(px(6.0))
                                        .truncate()
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted.opacity(0.6))
                                        .child(SharedString::from(if account.switchable {
                                            // Cursor is probed too (its usage DB is
                                            // read for the ACTIVE login only), so an
                                            // empty reading here is a real miss, not
                                            // a coverage gap. "Credentials
                                            // unavailable" stays reserved for a
                                            // live login we could not read.
                                            "Usage unavailable"
                                        } else {
                                            "Credentials unavailable"
                                        })),
                                )
                            } else {
                                el.child(
                                    div()
                                        .mt(px(6.0))
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.0))
                                        .children(
                                            usage_windows
                                                .iter()
                                                .map(|w| self.render_usage_meter(w, theme, now)),
                                        ),
                                )
                            }
                        } else {
                            el
                        }
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .gap(px(8.0))
                    .child(badges)
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_login_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let red_text = theme.danger_muted.opacity(0.9); // red-300
        let login = self.login.as_ref()?;
        let title = login.title();
        let url_link =
            |id: &'static str, label: &'static str, url: &str, cx: &mut Context<Self>| {
                let open_url = url.to_string();
                // "Reopen the …" text link (komet: `text-[12px]
                // text-muted-foreground/60 hover:underline`).
                div()
                    .id(id)
                    .mt(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_url);
                    }))
                    .child(SharedString::from(label))
            };
        let body: AnyElement = match login {
            LoginFlow::Starting { .. } => div()
                .mt(px(8.0))
                .child(popover::skeleton_rows(
                    "login-starting",
                    &theme,
                    2,
                    cx.entity_id(),
                    window,
                    cx,
                ))
                .into_any_element(),
            LoginFlow::PasteCode {
                harness,
                start,
                submitting,
                error,
                ..
            } => {
                let submitting = *submitting;
                let copy = match harness {
                    HarnessId::Antigravity => {
                        "A terminal-based Google sign-in has started. Approve access in \
                         the browser, then paste the authorization code Google shows \
                         you below. Your current login is untouched until you switch."
                    }
                    _ => {
                        "A browser window opened. Sign in to the account you want to add, \
                         approve access, then paste the code Anthropic shows you below. Your \
                         current login is untouched until you switch."
                    }
                };
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(&theme, copy)))
                    .child(url_link(
                        "login-open-url",
                        "Reopen the authorization page",
                        &start.url,
                        cx,
                    ))
                    .child(
                        div().mt(px(12.0)).child(
                            popover::dialog_field(self.code_input.clone().into_any_element())
                                .font_family(theme.font_mono.clone())
                                .text_size(px(13.0)),
                        ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(
                                    &theme,
                                    "Cancel",
                                    "login-cancel",
                                    cx.entity_id(),
                                )
                                .id("login-cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submitting {
                                        "Verifying…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .id("login-submit-code")
                                .when(submitting, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx))),
                            ),
                    )
                    .into_any_element()
            }
            LoginFlow::Browser {
                harness,
                start,
                message,
                error,
            } => {
                let has_error = error.is_some();
                let instructions = match harness {
                    HarnessId::Copilot => {
                        "Authorize GitHub in the browser to add this Copilot account. Komet keeps each connected account separate; your current selection is unchanged until you switch."
                    }
                    HarnessId::Codex => {
                        "Finish signing in to OpenAI in your browser. The new login is captured in an isolated profile — your current session is untouched until you switch."
                    }
                    HarnessId::Cursor => {
                        "Finish signing in to Cursor in your browser. Your current session is untouched until you switch."
                    }
                    _ => "Finish signing in to the provider in your browser.",
                };
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .mt(px(8.0))
                            .child(popover::dialog_body(&theme, instructions)),
                    )
                    .when_some(start.user_code.clone(), |el, code| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted)
                                        .child("Enter this one-time code on GitHub"),
                                )
                                .child(
                                    div()
                                        .text_size(px(20.0))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(theme.text)
                                        .child(code),
                                ),
                        )
                    })
                    .child(url_link(
                        "login-open-url-browser",
                        "Reopen the sign-in page",
                        &start.url,
                        cx,
                    ))
                    .when(!has_error, |el| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    crate::thinking_orbs::ThinkingOrb::new(
                                        crate::thinking_orbs::OrbState::Connecting,
                                        24.0,
                                    )
                                    .speed(1.25)
                                    .driven(
                                        cx.entity_id(),
                                        window,
                                        cx,
                                    ),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(message.clone().unwrap_or_else(|| {
                                            SharedString::from("Waiting for the browser…")
                                        })),
                                ),
                        )
                    })
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div().mt(px(16.0)).flex().flex_row().justify_end().child(
                            popover::btn_ghost(
                                &theme,
                                if has_error { "Close" } else { "Cancel" },
                                "login-cancel",
                                cx.entity_id(),
                            )
                            .id("login-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                    )
                    .into_any_element()
            }
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", viewport, card))
    }

    /// A ghost account row (komet settings.agents.tsx `SkeletonRow`): avatar,
    /// email line, two usage-meter ghosts, a badge — same geometry as the real
    /// row so loaded data lands without a layout jump. `dim` fades row two.
    fn render_skeleton_row(
        &self,
        _id: (&'static str, usize),
        dim: bool,
        first: bool,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::motion;
        let delta = motion::pulse_delta(&motion::KOMET_PULSE, cx.entity_id(), window, cx);
        let ghost = |w: gpui::Length, h: f32, round_full: bool| {
            div()
                .w(w)
                .h(px(h))
                .flex_none()
                .map(|el| {
                    if round_full {
                        el.rounded_full()
                    } else {
                        el.rounded(px(4.0))
                    }
                })
                .bg(crate::theme::ink(0.05))
        };
        let meters = div()
            .mt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .children((0..2).map(|_| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(ghost(px(48.0).into(), 9.0, false))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(56.0))
                            .max_w(px(230.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(crate::theme::ink(0.04)),
                    )
                    .child(ghost(px(64.0).into(), 9.0, false))
            }));
        let inner = div()
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .bg(crate::theme::ink(0.05)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ghost(px(176.0).into(), 13.0, false).max_w(gpui::relative(0.6)))
                    .child(meters),
            )
            .child(div().flex_none().flex().flex_col().items_end().child(ghost(
                px(64.0).into(),
                21.0,
                true,
            )));
        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .when(dim, |el| el.opacity(0.6))
            .child(inner.opacity(0.55 + 0.35 * motion::pulse_wave(delta)))
            .into_any_element()
    }
}

impl Render for AccountsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let dialog = self.render_login_dialog(window.viewport_size(), window, cx);
        // Driven by the load flag, not the `Loadable` state: a reload keeps the
        // previous snapshot on screen, so `Loading` no longer marks "in flight".
        let refreshing = self.refreshing;
        let account_count = self
            .snapshot
            .ready()
            .map(|s| s.accounts.len())
            .filter(|&n| n > 0);

        let provider_icon = |harness: HarnessId| match harness {
            HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
            HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
            HarnessId::Grok => (crate::icons::GROK_MARK, None),
            HarnessId::Hermes => (crate::icons::HERMES_MARK, None),
            HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
            HarnessId::Antigravity => (crate::icons::ANTIGRAVITY_MARK, None),
            HarnessId::Pi => (crate::icons::PI_MARK, None),
            HarnessId::Devin => (crate::icons::DEVIN_MARK, None),
            HarnessId::Muse => (crate::icons::MUSE_MARK, Some(crate::icons::muse_brand())),
            HarnessId::Copilot => (crate::icons::COPILOT_MARK, None),
            HarnessId::Cline => (crate::icons::CLINE_MARK, None),
            HarnessId::Qoder => (crate::icons::QODER_MARK, None),
            _ => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        // Brand mark inside a 24px centered box (komet: `grid size-6
        // place-items-center [&_svg]:size-4`).
        let provider_mark = |harness: HarnessId, theme: &Theme| {
            let (mark, tint) = provider_icon(harness);
            div()
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(mark)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
        };

        // One section per provider (komet settings.agents.tsx `ProviderSection`):
        // brand header + Add account, then the account rows card.
        let sections: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => PROVIDERS
                .into_iter()
                .map(|(harness, name, _cli)| {
                    let skeleton_id = match harness {
                        HarnessId::Codex => "accounts-skeleton-codex",
                        _ => "accounts-skeleton-claude",
                    };
                    div()
                        .mt(px(24.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(provider_mark(harness, &theme))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(name)),
                                ),
                        )
                        .child(
                            // Ghost rows shaped like real ones (row two dimmed)
                            // so the card keeps its size while data develops.
                            widgets::section_card(&theme)
                                .mt(px(8.0))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 0),
                                    false,
                                    true,
                                    &theme,
                                    window,
                                    cx,
                                ))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 1),
                                    true,
                                    false,
                                    &theme,
                                    window,
                                    cx,
                                )),
                        )
                        .into_any_element()
                })
                .collect(),
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    widgets::error_strip(&theme, message)
                        .id("accounts-load-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            // Retry IS the visit's first successful list — force usage.
                            this.load(usage_mode_for(LoadTrigger::Retry), cx)
                        }))
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Click to retry")),
                        )
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                PROVIDERS
                    .into_iter()
                    .enumerate()
                    .map(|(provider_index, (harness, name, cli))| {
                        let accounts = provider_accounts(&snapshot, harness);
                        // EVERY warning renders its own strip (komet maps them).
                        let warnings: Vec<String> = snapshot
                            .warnings
                            .iter()
                            .filter(|w| w.harness == harness)
                            .map(|w| w.message.clone())
                            .collect();
                        let rows: Vec<AnyElement> = accounts
                            .iter()
                            .enumerate()
                            .map(|(ix, account)| {
                                self.render_account_row(account, ix == 0, &theme, now, cx)
                            })
                            .collect();
                        let add_id: SharedString = format!("add-account-{name}").into();
                        let focused = self.focused_harness == Some(harness);
                        let card = widgets::section_card(&theme)
                            .mt(px(8.0))
                            .when(focused, |card| {
                                card.border_1().border_color(theme.accent.opacity(0.65))
                            });
                        let card = if rows.is_empty() {
                            let mut empty = div()
                                .px(px(20.0))
                                .py(px(24.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(8.0))
                                .text_center()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.75))
                                .child(SharedString::from(if cli_login_command(harness).is_some() {
                                    format!("{name} sign-in is managed by its own CLI.")
                                } else {
                                    format!("No {name} login detected on this device.")
                                }));
                            if let Some(command) = cli_login_command(harness) {
                                empty = empty
                                    .child(
                                        div()
                                            .max_w(px(460.0))
                                            .text_size(px(12.0))
                                            .text_color(theme.text_muted)
                                            .child(SharedString::from(login_instructions(harness))),
                                    )
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id(("accounts-copy-login-command", provider_index))
                                            .hover(|s| widgets::ghost_hover(&theme, s))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_login_command(harness, cx);
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::DOCUMENT)
                                                    .size(px(14.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child(SharedString::from(if self.copied_login_command
                                                == Some(harness)
                                            {
                                                "Command copied"
                                            } else {
                                                "Copy sign-in command"
                                            }))
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(theme.text_muted.opacity(0.5))
                                            .child(SharedString::from(command)),
                                    );
                            } else {
                                empty = empty.child(
                                    div()
                                        .max_w(px(460.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(format!(
                                            "Sign in with the {cli} CLI, or use Add account to connect another login."
                                        ))),
                                );
                            }
                            card.child(empty)
                        } else {
                            card.children(rows)
                        };
                        div()
                            .id(("accounts-provider-section", provider_index))
                            .when(focused, |el| {
                                el.child(
                                    div()
                                        .mb(px(6.0))
                                        .text_size(px(11.0))
                                        .text_color(theme.accent)
                                        .child(SharedString::from("Selected agent")),
                                )
                            })
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(provider_mark(harness, &theme))
                                    .child(
                                        div()
                                            .text_size(px(14.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(SharedString::from(name)),
                                    )
                                    .child(div().flex_1())
                                    .when(cli_login_command(harness).is_none(), |el| {
                                        el.child(
                                            widgets::ghost_action(&theme)
                                                .id(add_id)
                                                .hover(|s| widgets::ghost_hover(&theme, s))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.start_login(harness, cx);
                                                }))
                                                .child(
                                                    crate::icons::icon(crate::icons::ADD_CIRCLE)
                                                        .size(px(16.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child(SharedString::from("Add account")),
                                        )
                                    }),
                            )
                            .children(
                                warnings
                                    .into_iter()
                                    .map(|warning| widgets::warning_strip(&theme, warning)),
                            )
                            .child(card)
                            .into_any_element()
                    })
                    .collect()
            }
        };

        if self.scroll_target_pending
            && let Some(harness) = self.focused_harness
            && matches!(self.snapshot, Loadable::Ready(_))
            && let Some(provider_index) = PROVIDERS
                .iter()
                .position(|(provider, _, _)| *provider == harness)
        {
            self.page_scroll
                .scroll_to_item(provider_index + 2 + usize::from(self.error.is_some()));
            self.scroll_target_pending = false;
        }

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.page_scroll)
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .px(px(24.0))
            .pt(px(32.0))
            .pb(px(64.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::page_header(&theme, "Accounts", account_count))
                    .child(div().flex_1())
                    .child(
                        // `text-[12.5px]` + leading 16px Refresh icon,
                        // dimmed while a refresh is in flight (komet
                        // `disabled:opacity-50`).
                        widgets::ghost_action(&theme)
                            .id("accounts-refresh")
                            .flex_none()
                            .text_size(px(12.5))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .when(refreshing, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load(usage_mode_for(LoadTrigger::Refresh), cx)
                            }))
                            .child(
                                crate::icons::icon(crate::icons::REFRESH)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Refresh")),
                    )
                    .child(self.render_device_switcher(&theme, cx)),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Connect the agents that require an account or model-provider \
                         credentials. Claude Code, Codex, Antigravity and Cursor support \
                         account management here; other agents use their own CLI login.",
            ))
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    widgets::error_strip(&theme, message)
                        .id("accounts-action-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
            })
            .children(sections)
            // Footer note (komet: `mt-6 text-[12px] leading-relaxed
            // text-muted-foreground/60`).
            .child(
                div()
                    .mt(px(24.0))
                    .text_size(px(12.0))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(
                        "Switching rewrites the CLI\u{2019}s stored login, so new \
                         agent sessions use the selected account immediately. On \
                         macOS, an already-running Claude Code can hold the previous \
                         login for up to ~30 seconds (Keychain cache).",
                    )),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn first_load_of_a_visit_probes_on_a_cold_cache() {
        // The visit's first list must be able to hit the provider, or the open
        // renders "Usage unavailable" until a manual Refresh. `Cached` still
        // probes whenever the entry is missing or stale — which is the normal
        // case for a freshly opened page — without re-fetching a warm entry.
        assert_eq!(usage_mode_for(LoadTrigger::Mount), UsageMode::Cached);
        // A retry after a failed load is still the visit's first list.
        assert_eq!(usage_mode_for(LoadTrigger::Retry), UsageMode::Cached);
    }

    #[test]
    fn explicit_refresh_bypasses_the_cache() {
        // THE regression this guards: a Refresh clicked inside the 60s TTL used
        // to be served the previous numbers without a probe. `Fresh` ignores
        // the cache outright, so the click always reaches the engine network
        // path.
        assert_eq!(usage_mode_for(LoadTrigger::Refresh), UsageMode::Fresh);
        // A just-completed login must show the new account's real numbers, not
        // whatever the previous list cached for that slot id.
        assert_eq!(usage_mode_for(LoadTrigger::PostLogin), UsageMode::Fresh);
    }

    #[test]
    fn accounts_lists_every_login_required_agent() {
        let agents = [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Cursor,
            HarnessId::Devin,
            HarnessId::Grok,
            HarnessId::Hermes,
            HarnessId::Opencode,
            HarnessId::Pi,
            HarnessId::Antigravity,
            HarnessId::Muse,
            HarnessId::Copilot,
            HarnessId::Cline,
            HarnessId::Qoder,
        ];
        for agent in agents {
            assert!(
                requires_login(agent),
                "{} should be listed in Accounts",
                agent.display_name()
            );
            assert!(PROVIDERS.iter().any(|(provider, _, _)| *provider == agent));
        }
        assert!(!requires_login(HarnessId::Mock));
    }

    #[test]
    fn switch_and_forget_re_probe() {
        // The switch moved the LIVE login, so the cached entry describes the
        // account that is no longer active. `Cached` re-probes because the
        // engine invalidates the live entry on a switch.
        assert_eq!(usage_mode_for(LoadTrigger::PostAction), UsageMode::Cached);
    }

    #[test]
    fn no_trigger_stays_offline() {
        // Every visit-shaped load must be able to hit the network; only the
        // engine's internal bookkeeping lists are offline.
        for trigger in [
            LoadTrigger::Mount,
            LoadTrigger::Retry,
            LoadTrigger::Refresh,
            LoadTrigger::PostLogin,
            LoadTrigger::PostAction,
        ] {
            assert_ne!(usage_mode_for(trigger), UsageMode::Offline, "{trigger:?}");
        }
    }

    #[test]
    fn usage_thresholds_match_komet() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        // Within ~22h: a local clock time ("resets 3:45 PM").
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        // Beyond: a short weekday + time ("resets Mon 3:45 PM").
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {} {}",
                later.with_timezone(&Local).format("%a"),
                later.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
    }

    #[test]
    fn provider_grouping_puts_active_first() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["c2", "c1"], "active account leads");
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }

    #[test]
    fn usage_is_only_displayed_for_the_active_account() {
        let mut account = AgentAccount {
            id: "codex-1".into(),
            harness: HarnessId::Codex,
            email: None,
            plan_label: None,
            active: false,
            usage_windows: vec![AgentUsageWindow {
                label: "Week".into(),
                used_fraction: 0.42,
                resets_at: None,
                display_value: None,
            }],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };

        assert!(displayed_usage(&account).is_none());
        account.active = true;
        assert_eq!(displayed_usage(&account).unwrap()[0].label, "Week");
    }

    #[test]
    fn switching_moves_the_active_usage_to_the_selected_account() {
        let account = |id: &str, active: bool| AgentAccount {
            id: id.into(),
            harness: HarnessId::Codex,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let mut snapshot = AgentAccountsSnapshot {
            accounts: vec![account("old", true), account("selected", false)],
            warnings: vec![],
        };

        select_active_account(&mut snapshot, HarnessId::Codex, "selected");

        assert!(!snapshot.accounts[0].active);
        assert!(snapshot.accounts[1].active);
    }

    #[test]
    fn empty_refresh_keeps_the_last_known_usage_for_each_account() {
        let account = |usage_windows| AgentAccount {
            id: "codex-1".into(),
            harness: HarnessId::Codex,
            email: None,
            plan_label: None,
            active: true,
            usage_windows,
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let previous = AgentAccountsSnapshot {
            accounts: vec![account(vec![AgentUsageWindow {
                label: "Week".into(),
                used_fraction: 0.42,
                resets_at: None,
                display_value: None,
            }])],
            warnings: vec![],
        };
        let mut refreshed = AgentAccountsSnapshot {
            accounts: vec![account(vec![])],
            warnings: vec![],
        };

        retain_previous_usage(&mut refreshed, Some(&previous));

        assert_eq!(
            refreshed.accounts[0].usage_windows,
            previous.accounts[0].usage_windows
        );
    }
}
