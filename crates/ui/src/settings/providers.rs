//! Settings → Providers: one card per agent, folding what used to be two
//! pages. The header carries the enable toggle, the CLI install/update state
//! and a quick "Connect"; the body carries that provider's accounts — email,
//! plan badge, Active, Switch / Forget — plus the add-account dialogs
//! (paste-code and browser-poll flows). Agents whose sign-in lives in their
//! own CLI show the hand-off command instead.
//!
//! Enablement and logins are PER-DEVICE, so the page-header device switcher
//! retargets every RPC (`targetDeviceId` passthrough). Every call surfaces
//! failures as inline UI states rather than assuming the methods exist.

use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use komet_engine::harness_cli::HarnessCliStatus;
use komet_engine::registry::{HarnessDescriptor, descriptor_enabled};
use komet_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessId,
};
use komet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::pickers::visible_harnesses;
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

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

pub fn requires_login(harness: HarnessId) -> bool {
    PROVIDERS
        .iter()
        .any(|(provider, _, _)| *provider == harness)
}

pub fn supports_in_app_login(harness: HarnessId) -> bool {
    matches!(
        harness,
        HarnessId::ClaudeCode
            | HarnessId::Codex
            | HarnessId::Antigravity
            | HarnessId::Cursor
            | HarnessId::Copilot
            | HarnessId::Devin
            | HarnessId::Grok
            | HarnessId::Muse
            | HarnessId::Cline
            | HarnessId::Qoder
    )
}

/// Providers whose sign-in has no in-app flow yet: the page explains the CLI
/// command instead of showing an Add account button that cannot work.
pub fn cli_login_command(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::Hermes => Some("hermes"),
        HarnessId::Opencode => Some("opencode auth login"),
        HarnessId::Pi => Some("pi"),
        _ => None,
    }
}

pub fn login_instructions(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Hermes => "Run `hermes` and configure/sign in to a model provider.",
        HarnessId::Opencode => "Run `opencode auth login` and choose your model provider.",
        HarnessId::Pi => "Run `pi` and use `/login` to connect a model provider.",
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

/// One-line blurb per agent (the catalog descriptor doesn't carry one).
pub fn blurb(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Anthropic's coding agent, driven through the Claude Code CLI.",
        HarnessId::Codex => "OpenAI's coding agent, driven through the Codex CLI.",
        HarnessId::Cursor => "Cursor's coding agent, driven through the cursor-agent CLI.",
        HarnessId::Devin => "Devin's agent, driven through the Devin ACP flow.",
        HarnessId::Grok => "xAI's Grok Build agent (grok CLI).",
        HarnessId::Hermes => "Nous Research's Hermes Agent (hermes CLI).",
        HarnessId::Opencode => "The opencode coding agent, driven through the opencode CLI.",
        HarnessId::Pi => "The pi coding agent (pi CLI).",
        HarnessId::Antigravity => "Google Antigravity agent, driven through the agy CLI.",
        HarnessId::Muse => "Muse's session-based agent, driven through the MSP flow.",
        HarnessId::Copilot => "GitHub Copilot, driven through the official Copilot CLI.",
        HarnessId::Cline => "Cline's autonomous coding agent, driven through the cline CLI (ACP).",
        HarnessId::Qoder => "Alibaba's Qoder CLI, driven over its native ACP server.",
        HarnessId::Mock => "Scripted test harness.",
    }
}

/// The CLI named in the not-installed hint.
pub fn cli_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor-agent",
        HarnessId::Devin => "devin",
        HarnessId::Grok => "grok",
        HarnessId::Hermes => "hermes",
        HarnessId::Opencode => "opencode",
        HarnessId::Pi => "pi",
        HarnessId::Antigravity => "agy",
        HarnessId::Muse => "muse",
        HarnessId::Copilot => "copilot",
        HarnessId::Cline => "cline",
        HarnessId::Qoder => "qoder",
        HarnessId::Mock => "mock",
    }
}

fn harness_display_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Pi => "Pi",
        HarnessId::Antigravity => "Antigravity",
        HarnessId::Muse => "Muse",
        HarnessId::Copilot => "Copilot",
        HarnessId::Cline => "Cline",
        HarnessId::Qoder => "Qoder",
        HarnessId::Mock => "Mock",
    }
}

/// Connected/unknown harnesses, derived from the accounts snapshot.
#[derive(Default)]
struct AgentConnectionStatus {
    connected: HashSet<HarnessId>,
    unknown: HashSet<HarnessId>,
}

impl From<AgentAccountsSnapshot> for AgentConnectionStatus {
    fn from(snapshot: AgentAccountsSnapshot) -> Self {
        Self {
            connected: snapshot
                .accounts
                .into_iter()
                .map(|account| account.harness)
                .collect(),
            unknown: snapshot
                .warnings
                .into_iter()
                .map(|warning| warning.harness)
                .collect(),
        }
    }
}

/// A quick "Connect" is offered only where a login we can start exists and the
/// provider is neither connected nor in an unknown state.
fn show_connect_button(harness: HarnessId, status: Option<&AgentConnectionStatus>) -> bool {
    supports_in_app_login(harness)
        && status.is_some_and(|status| {
            !status.connected.contains(&harness) && !status.unknown.contains(&harness)
        })
}

fn newly_available_cli_update(
    statuses: &[HarnessCliStatus],
    announced: &HashSet<(HarnessId, String)>,
    enabled: &HashSet<HarnessId>,
) -> Option<HarnessCliStatus> {
    statuses
        .iter()
        .find(|status| {
            enabled.contains(&status.harness)
                && status.update_available
                && status
                    .latest_version
                    .as_ref()
                    .is_some_and(|version| !announced.contains(&(status.harness, version.clone())))
        })
        .cloned()
}

/// The collapsed card's second line: how many accounts, and which one is live.
fn account_summary(accounts: &[&AgentAccount]) -> SharedString {
    if accounts.is_empty() {
        return SharedString::default();
    }
    let noun = if accounts.len() == 1 {
        "account"
    } else {
        "accounts"
    };
    let active = accounts.iter().find(|account| account.active);
    match active {
        Some(account) => {
            let email: &str = account
                .email
                .as_deref()
                .or(account.display_name.as_deref())
                .unwrap_or("Unknown account");
            SharedString::from(format!("{} {noun} · Active: {email}", accounts.len()))
        }
        None => SharedString::from(format!("{} {noun} · Not connected", accounts.len())),
    }
}

/// The last enabled agent cannot be switched off — the composer needs
/// something to run (mirrors the engine guard).
fn toggle_interactive(installed: bool, enabled: bool, enabled_count: usize) -> bool {
    installed && !(enabled && enabled_count == 1)
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
            HarnessId::Grok => "Add Grok account",
            HarnessId::Muse => "Add Muse account",
            HarnessId::Cline => "Add Cline account",
            HarnessId::Qoder => "Add Qoder account",
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

pub struct ProvidersPage {
    state: Entity<AppState>,
    /// Which device's harnesses and logins are shown; `None` = this device (no
    /// passthrough). Retargeted by the page-header device switcher; every RPC
    /// resolves it again through [`Self::with_target`].
    target_device: Option<String>,
    target_generation: u64,
    device_menu: popover::Popup<()>,
    /// One card per entry; the authoritative list of agents.
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    accounts: Loadable<AgentAccountsSnapshot>,
    /// A `ListHarnesses`/`ListAgentAccounts` is in flight. Kept separate from
    /// the `Loadable` states so a refresh dims the button and leaves the cards
    /// on screen instead of collapsing them to skeletons.
    refreshing: bool,
    cli_status: HashMap<HarnessId, HarnessCliStatus>,
    cli_update_prompt: Option<HarnessCliStatus>,
    announced_cli_updates: HashSet<(HarnessId, String)>,
    checking_updates: bool,
    managing_cli: Option<HarnessId>,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    /// Failure of the accounts list, shown apart from the harness one: a
    /// failed read must not be painted as "not connected".
    accounts_error: Option<SharedString>,
    copied_login_command: Option<HarnessId>,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    load_generation: u64,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    toggle_task: Option<Task<()>>,
    cli_status_task: Option<Task<()>>,
    manage_cli_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl ProvidersPage {
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
            target_generation: 0,
            device_menu: popover::Popup::default(),
            harnesses: Loadable::Idle,
            accounts: Loadable::Idle,
            refreshing: false,
            cli_status: HashMap::new(),
            cli_update_prompt: None,
            announced_cli_updates: HashSet::new(),
            checking_updates: false,
            managing_cli: None,
            busy_account: None,
            login: None,
            error: None,
            accounts_error: None,
            copied_login_command: None,
            code_input,
            load_task: None,
            load_generation: 0,
            action_task: None,
            poll_task: None,
            toggle_task: None,
            cli_status_task: None,
            manage_cli_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        page.load_harnesses(cx);
        page.load_accounts(cx);
        page
    }

    fn copy_login_command(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if let Some(command) = cli_login_command(harness) {
            cx.write_to_clipboard(ClipboardItem::new_string(command.to_string()));
            self.copied_login_command = Some(harness);
            cx.notify();
        }
    }

    /// Retarget the page at another device's harnesses and logins: every RPC
    /// is relay-forwardable, so the whole page — list, toggle, switch, forget,
    /// login flows — follows the passthrough.
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
        self.target_generation = self.target_generation.wrapping_add(1);
        // A different device = a different world: drop in-flight login/action
        // state and every cached status, then reload.
        self.login = None;
        self.busy_account = None;
        self.error = None;
        self.accounts_error = None;
        self.cli_status.clear();
        self.cli_update_prompt = None;
        self.announced_cli_updates.clear();
        self.managing_cli = None;
        self.harnesses = Loadable::Idle;
        self.accounts = Loadable::Idle;
        self.load_harnesses(cx);
        self.load_accounts(cx);
    }

    /// Params with the `targetDeviceId` passthrough merged in. Resolved per
    /// call, so a device picked elsewhere in the app is honoured without the
    /// page having to observe it.
    fn with_target(&self, value: serde_json::Value, cx: &App) -> serde_json::Value {
        let mut value = value;
        let target = self
            .target_device
            .clone()
            .or_else(|| self.state.read(cx).harness_target_device());
        if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
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
                .id("providers-device-switcher")
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
                "providers-device-menu",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    /// `ListHarnesses` against the target device (installed probe + enabled
    /// set both come from where the CLIs actually live). The reply is the
    /// card list; CLI versions and accounts follow once it lands.
    fn load_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if matches!(self.harnesses, Loadable::Idle) {
                self.harnesses = Loadable::Error("Engine not connected".into());
            }
            return;
        };
        let params = self.with_target(serde_json::json!({}), cx);
        let generation = self.target_generation;
        if matches!(self.harnesses, Loadable::Idle) {
            self.harnesses = Loadable::Loading;
        }
        self.refreshing = true;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LIST_HARNESSES, params).await;
            this.update(cx, |page, cx| {
                if page.target_generation != generation {
                    return;
                }
                page.refreshing = false;
                page.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => {
                            page.error = None;
                            Loadable::Ready(list)
                        }
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if matches!(page.harnesses, Loadable::Ready(_)) {
                    page.load_cli_status(cx, false, false);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// `GetHarnessCliStatus`: versions for the install/update line. `refresh`
    /// forces the probe; `check_latest` is the explicit update check.
    fn load_cli_status(&mut self, cx: &mut Context<Self>, check_latest: bool, force: bool) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.checking_updates = false;
            return;
        };
        let params = self.with_target(
            serde_json::json!({ "checkLatest": check_latest, "force": force }),
            cx,
        );
        let generation = self.target_generation;
        self.checking_updates = check_latest;
        self.cli_status_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_HARNESS_CLI_STATUS, params)
                .await;
            this.update(cx, |page, cx| {
                if page.target_generation != generation {
                    return;
                }
                page.checking_updates = false;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessCliStatus>>(value) {
                        Ok(statuses) => {
                            if check_latest
                                && page.cli_update_prompt.is_none()
                                && let Some(status) = newly_available_cli_update(
                                    &statuses,
                                    &page.announced_cli_updates,
                                    &page.enabled_harness_ids(&statuses),
                                )
                            {
                                if let Some(version) = status.latest_version.clone() {
                                    page.announced_cli_updates.insert((status.harness, version));
                                }
                                page.cli_update_prompt = Some(status);
                            }
                            page.cli_status = statuses
                                .into_iter()
                                .map(|status| (status.harness, status))
                                .collect();
                        }
                        Err(err) => page.error = Some(err.to_string().into()),
                    },
                    Err(err) => page.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.load_cli_status(cx, true, true);
    }

    /// Enabled harnesses for prompt filtering; falls back to every status when
    /// the descriptor list has not landed yet (never suppress a real update).
    fn enabled_harness_ids(&self, statuses: &[HarnessCliStatus]) -> HashSet<HarnessId> {
        match &self.harnesses {
            Loadable::Ready(list) => list
                .iter()
                .filter(|descriptor| descriptor_enabled(descriptor))
                .map(|descriptor| descriptor.id)
                .collect(),
            _ => statuses.iter().map(|status| status.harness).collect(),
        }
    }

    fn render_cli_update_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.cli_update_prompt.as_ref()?;
        let harness = status.harness;
        let name = harness_display_name(harness);
        let theme = Theme::of(cx).clone();
        let version_copy = match (&status.installed_version, &status.latest_version) {
            (Some(installed), Some(latest)) => {
                format!("Version {installed} is installed; version {latest} is available.")
            }
            _ => "A newer version is available.".to_string(),
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(
                &theme,
                &format!("{name} update available"),
            ))
            .child(
                div()
                    .mt(px(8.0))
                    .child(popover::dialog_body(
                        &theme,
                        format!("{version_copy} New models often need the latest version."),
                    )),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Later", "cli-update-later", cx.entity_id())
                            .id("cli-update-later")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cli_update_prompt = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Update")
                            .id("cli-update-now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cli_update_prompt = None;
                                this.manage_cli(harness, cx);
                            })),
                    ),
            )
            .into_any_element();
        Some(popover::modal("harness-cli-update-dialog", viewport, card))
    }

    /// Flip one harness on the target device. The reply carries the device's
    /// fresh catalog, so the cards repaint from the authoritative state in one
    /// round trip; refusals (engine guards) land in the error strip.
    fn toggle(&mut self, harness: HarnessId, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(
            serde_json::json!({
                "harness": harness,
                "enabled": enabled,
            }),
            cx,
        );
        self.error = None;
        self.toggle_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_HARNESS_ENABLED, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => {
                        if let Ok(list) = serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                            page.harnesses = Loadable::Ready(list);
                        }
                        // The composer caches its catalog per space — poke
                        // every Pickers to re-fetch, or the rail keeps the
                        // old set until restart.
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(err) => page.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn manage_cli(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "harness": harness }), cx);
        let generation = self.target_generation;
        self.error = None;
        self.managing_cli = Some(harness);
        self.manage_cli_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::MANAGE_HARNESS_CLI, params)
                .await;
            this.update(cx, |page, cx| {
                if page.target_generation != generation {
                    return;
                }
                page.managing_cli = None;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => {
                            page.harnesses = Loadable::Ready(list);
                            page.load_cli_status(cx, false, false);
                            crate::pickers::bump_harness_catalog(cx);
                        }
                        Err(err) => page.error = Some(err.to_string().into()),
                    },
                    Err(err) => page.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// `ListAgentAccounts` (never a usage probe): the accounts shown inside
    /// each card. A failed read keeps the last good snapshot on screen and
    /// reports itself separately from the harness list.
    fn load_accounts(&mut self, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.refreshing = false;
            if matches!(self.accounts, Loadable::Ready(_)) {
                self.accounts_error = Some("Refresh failed: engine not connected".into());
            } else {
                self.accounts = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        // Only the first load has nothing to show yet; afterwards the previous
        // snapshot stays rendered while the new one is in flight.
        if matches!(self.accounts, Loadable::Idle) {
            self.accounts = Loadable::Loading;
        }
        self.refreshing = true;
        let params = self.with_target(serde_json::json!({ "usageMode": "none" }), cx);
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
                let previous = page.accounts.ready().cloned();
                let parsed = result.map_err(|err| err.to_string()).and_then(|value| {
                    serde_json::from_value::<AgentAccountsSnapshot>(value)
                        .map_err(|err| err.to_string())
                });
                page.accounts = match parsed {
                    Ok(snapshot) => {
                        page.accounts_error = None;
                        Loadable::Ready(snapshot)
                    }
                    Err(message) if previous.is_some() => {
                        // Keep the last successful account snapshot visible
                        // when a background refresh fails transiently.
                        page.accounts_error = Some(format!("Refresh failed: {message}").into());
                        Loadable::Ready(previous.expect("checked above"))
                    }
                    Err(message) => {
                        page.accounts_error = Some(message.clone().into());
                        Loadable::Error(message)
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Switch / Forget an account.
    /// One card per agent, in `ListHarnesses` order. The header carries the
    /// decision — installed, enabled, connected — and the body the accounts;
    /// agents whose sign-in lives in their own CLI show the hand-off there.
    fn cards(
        &self,
        descriptors: &[HarnessDescriptor],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        use crate::settings::widgets;
        let enabled_count = descriptors
            .iter()
            .filter(|descriptor| descriptor_enabled(descriptor))
            .count();
        let connection = self
            .accounts
            .ready()
            .cloned()
            .map(AgentConnectionStatus::from);
        descriptors
            .iter()
            .enumerate()
            .map(|(ix, descriptor)| {
                let harness = descriptor.id;
                let name = harness_display_name(harness);
                let installed = descriptor.installed;
                let enabled = descriptor_enabled(descriptor);
                let interactive = toggle_interactive(installed, enabled, enabled_count);
                let cli_status = self.cli_status.get(&harness);
                let update_available = cli_status.is_some_and(|status| status.update_available);
                let show_manage_button = cli_status.is_some_and(|status| {
                    status.managed
                        && (!installed
                            || status.update_available
                            || status.latest_version.is_none())
                });
                let managing = self.managing_cli == Some(harness);
                let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);

                let accounts: Vec<&AgentAccount> = self
                    .accounts
                    .ready()
                    .map(|snapshot| provider_accounts(snapshot, harness))
                    .unwrap_or_default();
                let summary = account_summary(&accounts);

                let mut meta: Vec<AnyElement> =
                    vec![div().child(SharedString::from(blurb(harness))).into_any_element()];
                if !installed {
                    meta.push(
                        div()
                            .text_color(theme.warning_muted.opacity(0.9))
                            .child(SharedString::from(format!(
                                "Install the {} CLI to enable",
                                cli_name(harness)
                            )))
                            .into_any_element(),
                    );
                }
                if let Some(status) = cli_status {
                    if installed && status.installed_version.is_none() {
                        meta.push(
                            div()
                                .text_color(theme.warning_muted.opacity(0.9))
                                .child(SharedString::from(
                                    "The installed CLI version could not be read",
                                ))
                                .into_any_element(),
                        );
                    }
                    if let Some(installed_version) = &status.installed_version {
                        let version_text = if status.update_available {
                            format!(
                                "v{installed_version} → v{} available",
                                status.latest_version.as_deref().unwrap_or_default()
                            )
                        } else {
                            format!("Version {installed_version}")
                        };
                        meta.push(
                            div()
                                .text_color(if status.update_available {
                                    theme.warning_muted
                                } else {
                                    theme.text_muted.opacity(0.75)
                                })
                                .child(SharedString::from(version_text))
                                .into_any_element(),
                        );
                    } else if let Some(latest_version) = &status.latest_version {
                        meta.push(
                            div()
                                .text_color(theme.text_muted.opacity(0.75))
                                .child(SharedString::from(format!(
                                    "Latest version {latest_version}"
                                )))
                                .into_any_element(),
                        );
                    }
                    if status.managed && status.check_error.is_some() {
                        meta.push(
                            div()
                                .text_color(theme.warning_muted.opacity(0.9))
                                .child(SharedString::from(format!(
                                    "Update check failed: {}",
                                    status.check_error.as_deref().unwrap_or_default()
                                )))
                                .into_any_element(),
                        );
                    } else if status.managed
                        && installed
                        && status.latest_version.is_none()
                        && status.installed_version.is_some()
                    {
                        meta.push(
                            div()
                                .text_color(theme.text_muted.opacity(0.65))
                                .child(SharedString::from(
                                    "Automatic update checks aren't available for this CLI",
                                ))
                                .into_any_element(),
                        );
                    }
                }
                if !summary.is_empty() {
                    meta.push(
                        div()
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(summary)
                            .into_any_element(),
                    );
                }

                let tile = div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(icon_path)
                            .size(px(16.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    );

                let mut actions = div().flex().flex_row().items_center().gap(px(8.0));
                if show_connect_button(harness, connection.as_ref()) {
                    actions = actions.child(
                        widgets::ghost_action(theme)
                            .id(("provider-connect", ix))
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.start_login(harness, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::KEY_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Connect")),
                    );
                }
                if show_manage_button {
                    let label = if managing {
                        "Working…"
                    } else if installed {
                        "Update"
                    } else {
                        "Install"
                    };
                    let action_icon = if installed && update_available {
                        crate::icons::REFRESH
                    } else {
                        crate::icons::ADD_CIRCLE
                    };
                    actions = actions.child(
                        widgets::ghost_action(theme)
                            .id(("provider-manage-cli", ix))
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .when(!managing, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.manage_cli(harness, cx);
                                }))
                            })
                            .child(
                                crate::icons::icon(action_icon)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(label)),
                    );
                }

                // Body: this provider's warnings, then its accounts, then the
                // auth entry point. A failed accounts read is never painted as
                // "not connected" — the page-level strip explains it instead.
                let mut body = div().flex().flex_col();
                if let Loadable::Ready(snapshot) = &self.accounts {
                    for warning in snapshot
                        .warnings
                        .iter()
                        .filter(|warning| warning.harness == harness)
                    {
                        body = body.child(widgets::warning_strip(theme, warning.message.clone()));
                    }
                }
                let accounts_known = matches!(self.accounts, Loadable::Ready(_));
                if accounts_known && accounts.is_empty() {
                    let copy = if cli_login_command(harness).is_some() {
                        format!("{name} sign-in is managed by its own CLI.")
                    } else if supports_in_app_login(harness) {
                        format!("No {name} login detected on this device.")
                    } else {
                        String::new()
                    };
                    if !copy.is_empty() {
                        body = body.child(
                            div()
                                .px(px(20.0))
                                .py(px(18.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(8.0))
                                .text_center()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.75))
                                .child(SharedString::from(copy)),
                        );
                    }
                }
                if accounts_known {
                    body = body.children(
                        accounts
                            .iter()
                            .enumerate()
                            .map(|(row, account)| {
                                self.render_account_row(account, row == 0, theme, cx)
                            })
                            .collect::<Vec<AnyElement>>(),
                    );
                } else if !matches!(self.accounts, Loadable::Error(_)) {
                    body = body.child(
                        div()
                            .px(px(20.0))
                            .py(px(14.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from("Loading accounts…")),
                    );
                }

                if supports_in_app_login(harness) {
                    body = body.child(
                        div()
                            .px(px(20.0))
                            .py(px(12.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .when(!accounts.is_empty(), |el| {
                                el.border_t_1().border_color(theme.border)
                            })
                            .child(
                                widgets::ghost_action(theme)
                                    .id(("provider-add-account", ix))
                                    .hover(|s| widgets::ghost_hover(theme, s))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.start_login(harness, cx);
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::ADD_CIRCLE)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Add account")),
                            ),
                    );
                } else if let Some(command) = cli_login_command(harness) {
                    body = body.child(
                        div()
                            .px(px(20.0))
                            .py(px(14.0))
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .when(!accounts.is_empty(), |el| {
                                el.border_t_1().border_color(theme.border)
                            })
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(login_instructions(harness))),
                            )
                            .child(
                                widgets::ghost_action(theme)
                                    .id(("provider-copy-login-command", ix))
                                    .hover(|s| widgets::ghost_hover(theme, s))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.copy_login_command(harness, cx);
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::DOCUMENT)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from(
                                        if self.copied_login_command == Some(harness) {
                                            "Command copied"
                                        } else {
                                            "Copy sign-in command"
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(SharedString::from(command)),
                            ),
                    );
                }

                widgets::section_card(theme)
                    .id(("provider-card", ix))
                    .child(
                        div()
                            .px(px(20.0))
                            .py(px(14.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(14.0))
                            .child(tile)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                div().flex_1().min_w_0().child(
                                                    widgets::row_title(theme, descriptor.name.clone()),
                                                ),
                                            )
                                            .child(actions),
                                    )
                                    .child(widgets::meta_line(theme, meta)),
                            )
                            .child(
                                widgets::toggle_switch(theme, enabled)
                                    .id(("provider-toggle", ix))
                                    .when(interactive, |el| {
                                        el.cursor_pointer().on_click(cx.listener(
                                            move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.toggle(harness, !enabled, cx);
                                            },
                                        ))
                                    }),
                            ),
                    )
                    .child(body)
                    .into_any_element()
            })
            .collect()
    }

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
        let params = self.with_target(
            serde_json::json!({
                "id": account.id,
                "accountId": account.id,
                "harness": account.harness,
            }),
            cx,
        );
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
                        if is_switch && let Loadable::Ready(snapshot) = &mut page.accounts {
                            select_active_account(snapshot, account_harness, &account_id);
                        }
                        page.load_accounts(cx);
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
        let params = self.with_target(serde_json::json!({ "harness": harness }), cx);
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
                        if !start.url.is_empty() {
                            cx.open_url(&start.url);
                        }
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
        let params = self.with_target(serde_json::json!({ "loginId": login_id, "code": code }), cx);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load_accounts(cx);
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
        let params = self.with_target(serde_json::json!({ "loginId": login_id }), cx);
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
                                page.load_accounts(cx);
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
            let params = self.with_target(serde_json::json!({ "loginId": login_id }), cx);
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

    /// One account row (komet settings.agents.tsx `AccountRow`): initial
    /// avatar and email left; badges over the Switch/Forget actions
    /// right-anchored.
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
                    .child(widgets::row_title(theme, email)),
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
                    HarnessId::Devin => {
                        "The Devin CLI started a manual sign-in. Approve access in the \
                         browser, then paste the token Devin shows you below."
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
                    HarnessId::Grok => {
                        "Finish signing in to Grok in your browser. The CLI writes the new login to its own store once you approve the code."
                    }
                    HarnessId::Muse => {
                        "Approve the Meta account code in your browser. Komet waits for the Muse CLI to finish the sign-in."
                    }
                    HarnessId::Cline => {
                        "Finish signing in to Cline in your browser. The Cline CLI stores the account and Komet picks it up automatically."
                    }
                    HarnessId::Qoder => {
                        "Finish signing in to Qoder in your browser. Komet waits for the Qoder CLI to store the new account."
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
                                        .child(SharedString::from(match *harness {
                                            HarnessId::Copilot => {
                                                "Enter this one-time code on GitHub"
                                            }
                                            _ => "Enter this one-time code in your browser",
                                        })),
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

}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let dialog = self.render_login_dialog(window.viewport_size(), window, cx);
        let update_dialog = self.render_cli_update_dialog(window.viewport_size(), cx);
        // Driven by the load flags, not the `Loadable` states: a reload keeps
        // the previous cards on screen, so `Loading` no longer means in flight.
        let refreshing = self.refreshing;

        let body: AnyElement = match &self.harnesses {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "providers-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    window,
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => div()
                .child(widgets::error_strip(&theme, message.clone()))
                .child(
                    widgets::ghost_action(&theme)
                        .id("providers-retry")
                        .mt(px(8.0))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.load_harnesses(cx);
                            this.load_accounts(cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(list) => {
                let cards = self.cards(&visible_harnesses(list), &theme, cx);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .children(cards)
                    .into_any_element()
            }
        };

        div()
            .id("providers-page")
            .size_full()
            .overflow_y_scroll()
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
                    .child(widgets::page_header(&theme, "Providers", None))
                    .child(div().flex_1())
                    .child(
                        widgets::ghost_action(&theme)
                            .id("providers-check-updates")
                            .flex_none()
                            .text_size(px(12.5))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .when(!self.checking_updates, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.check_updates(cx)))
                            })
                            .child(crate::icons::icon(crate::icons::REFRESH).size(px(14.0)))
                            .child(SharedString::from(if self.checking_updates {
                                "Checking for updates…"
                            } else {
                                "Check for updates"
                            })),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("providers-refresh")
                            .flex_none()
                            .text_size(px(12.5))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .when(refreshing, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_harnesses(cx);
                                this.load_accounts(cx);
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
                "Enable the agents the composer offers and connect their accounts. \
                 Enablement and logins are per device — switch devices in the header. \
                 Open a card to see its saved accounts; agents that sign in through \
                 their own CLI show the command to run instead.",
            ))
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    widgets::error_strip(&theme, message)
                        .id("providers-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
            })
            .when_some(self.accounts_error.clone(), |el, message| {
                el.child(
                    widgets::error_strip(&theme, message)
                        .id("providers-accounts-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.accounts_error = None;
                            cx.notify();
                        })),
                )
            })
            .child(body)
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
            .when_some(update_dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_login_required_agent_is_a_provider_card() {
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
                "{} should be listed in Providers",
                agent.display_name()
            );
            assert!(PROVIDERS.iter().any(|(provider, _, _)| *provider == agent));
        }
        assert!(!requires_login(HarnessId::Mock));
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
    fn account_summary_names_the_active_account() {
        let account = |id: &str, email: &str, active: bool| AgentAccount {
            id: id.into(),
            harness: HarnessId::ClaudeCode,
            email: Some(email.into()),
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let active = account("a", "me@x.dev", true);
        let other = account("b", "other@x.dev", false);
        assert_eq!(
            account_summary(&[&active, &other]).to_string(),
            "2 accounts · Active: me@x.dev"
        );
        assert_eq!(
            account_summary(&[&other]).to_string(),
            "1 account · Not connected"
        );
        assert_eq!(account_summary(&[]).to_string(), "");
    }

    #[test]
    fn the_last_enabled_agent_cannot_be_switched_off() {
        assert!(!toggle_interactive(true, true, 1));
        assert!(toggle_interactive(true, true, 2));
        assert!(toggle_interactive(true, false, 1));
        assert!(!toggle_interactive(false, false, 3));
    }

    #[test]
    fn connect_is_only_offered_for_unconnected_app_managed_accounts() {
        let status = AgentConnectionStatus {
            connected: HashSet::from([HarnessId::ClaudeCode]),
            unknown: HashSet::from([HarnessId::Antigravity]),
        };

        assert!(!show_connect_button(HarnessId::ClaudeCode, Some(&status)));
        assert!(show_connect_button(HarnessId::Codex, Some(&status)));
        // Grok has an in-app browser login, so an unconnected Grok offers it.
        assert!(show_connect_button(HarnessId::Grok, Some(&status)));
        assert!(!show_connect_button(HarnessId::Antigravity, Some(&status)));
        // Hermes signs in through its own CLI: never a Connect button.
        assert!(!show_connect_button(HarnessId::Hermes, Some(&status)));
        assert!(!show_connect_button(HarnessId::Codex, None));
    }

    #[test]
    fn newly_available_cli_updates_are_prompted_once_per_version() {
        let status = HarnessCliStatus {
            harness: HarnessId::ClaudeCode,
            installed_version: Some("2.1.320".into()),
            latest_version: Some("2.2.209".into()),
            managed: true,
            update_available: true,
            check_error: None,
        };

        let enabled = HashSet::from([HarnessId::ClaudeCode]);
        assert_eq!(
            newly_available_cli_update(std::slice::from_ref(&status), &HashSet::new(), &enabled)
                .map(|status| status.harness),
            Some(HarnessId::ClaudeCode)
        );
        let announced = HashSet::from([(HarnessId::ClaudeCode, "2.2.209".to_string())]);
        assert!(
            newly_available_cli_update(std::slice::from_ref(&status), &announced, &enabled)
                .is_none()
        );
        // A disabled harness never prompts, even with a newer release.
        assert!(newly_available_cli_update(&[status], &HashSet::new(), &HashSet::new()).is_none());
    }

    #[test]
    fn switching_moves_the_active_account() {
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
        let baseline = AgentAccountsSnapshot {
            accounts: vec![account("old", true), account("selected", false)],
            warnings: vec![],
        };
        let flags = |s: &AgentAccountsSnapshot| -> Vec<bool> {
            s.accounts.iter().map(|a| a.active).collect()
        };

        let mut snapshot = baseline.clone();
        select_active_account(&mut snapshot, HarnessId::Codex, "selected");
        assert_eq!(
            flags(&snapshot),
            [false, true],
            "selected activated, previous released"
        );

        // Unknown id: the function early-returns, no flags move.
        let mut snapshot = baseline.clone();
        select_active_account(&mut snapshot, HarnessId::Codex, "missing");
        assert_eq!(flags(&snapshot), flags(&baseline));

        // Foreign harness: matches no account either, flags stay put.
        select_active_account(&mut snapshot, HarnessId::ClaudeCode, "selected");
        assert_eq!(flags(&snapshot), flags(&baseline));
    }
}
