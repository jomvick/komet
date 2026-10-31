//! Settings → Agents: enable/disable harnesses (the t3code models-page
//! arrangement — one card row per agent with a trailing toggle).
//!
//! The state is PER-DEVICE and lives on the engine (`harness-prefs.json` in
//! its data dir): CLI installs are per-device, so enablement is too. The
//! page-header device switcher (the Accounts pattern) retargets both the
//! `ListHarnesses` probe and the `SetHarnessEnabled` writes at any registered
//! device over the relay-forwarded RPCs.
//!
//! Only Claude Code and Codex ship enabled; the rest are opt-in. A harness
//! whose CLI is missing on the target device offers its official installer
//! when Komet has a verified install source, while its enable toggle stays
//! inert. The engine enforces the same gate (plus "can't disable the last
//! enabled harness") where the state lives, so stale toggles self-correct.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Task,
    Window, div, prelude::*, px,
};

use komet_engine::harness_cli::HarnessCliStatus;
use komet_engine::registry::{HarnessDescriptor, descriptor_enabled};
use komet_proto::{AgentAccountsSnapshot, HarnessId, HarnessInteractionSurface};
use komet_rpc::methods;

use crate::pickers::visible_harnesses;
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessesEvent {
    Connect(HarnessId),
}

/// One-line blurb per agent (the t3code models page pairs every toggle row
/// with a description; the catalog descriptor doesn't carry one).
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

/// The declared interaction surface, phrased for the settings row — what the
/// adapters carry, never what an agent might gain later. An absent signal is
/// said out loud ("not visible"), because silence on a capability the user is
/// deciding about reads as approval.
///
/// `None` (empty) is the unknown case: a catalog from an engine predating the
/// surface field, which must not be presented as "unsupported".
pub fn surface_phrases(surface: &HarnessInteractionSurface) -> Vec<&'static str> {
    use komet_proto::{
        CompactionSurface as C, PlanSurface as P, ProgressSurface as T, QuestionSurface as Q,
        SubagentSurface as S,
    };
    if *surface == HarnessInteractionSurface::default() {
        return Vec::new();
    }
    vec![
        match surface.questions {
            Q::None => "Questions: not supported",
            Q::PermissionShaped => "Questions: as permission requests",
            Q::Elicitation => "Questions: agent prompts back",
            Q::Native => "Questions: the agent's own tool",
        },
        match surface.compaction {
            C::None => "Compaction: not visible",
            C::BoundaryOnly => "Compaction: boundary only",
            C::Lifecycle => "Compaction: live status",
            C::LifecycleWithSummary => "Compaction: status + summary",
        },
        match surface.subagents {
            S::None => "Subagents: not visible",
            S::Tagged => "Subagents: shown inline",
            S::TaggedIfEmitted => "Subagents: shown when emitted",
            S::NestedSession => "Subagents: separate session",
        },
        match surface.plans {
            P::None => "Plan: not visible",
            P::ToolChip => "Plan: in the transcript",
            P::LivePanel => "Plan: card above the composer",
        },
        match surface.tool_progress {
            T::None => "Tool output: not streamed",
            T::Final => "Tool output: result only",
            T::Streaming => "Tool output: live while running",
        },
    ]
}

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

fn show_connect_button(harness: HarnessId, status: Option<&AgentConnectionStatus>) -> bool {
    crate::settings::providers::cli_login_command(harness).is_none()
        && crate::settings::providers::requires_login(harness)
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

pub struct HarnessesPage {
    state: Entity<AppState>,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    /// Which device's harnesses are shown/edited; `None` = this device (no
    /// passthrough). Retargeted by the page-header device switcher.
    target_device: Option<String>,
    target_generation: u64,
    device_menu_open: bool,
    /// Whether the menu was open when the trigger press began — the menu's
    /// `on_mouse_down_out` closes it on that same press, so by click time a
    /// plain toggle would reopen (the [`popover::Popup`] press note, for
    /// this page's bool-state menu).
    device_menu_pressed_open: bool,
    /// Last refused/failed toggle (engine guards), shown in the error strip.
    error: Option<String>,
    cli_status: HashMap<HarnessId, HarnessCliStatus>,
    cli_update_prompt: Option<HarnessCliStatus>,
    announced_cli_updates: HashSet<(HarnessId, String)>,
    connection_status: Option<AgentConnectionStatus>,
    checking_updates: bool,
    managing_cli: Option<HarnessId>,
    load_task: Option<Task<()>>,
    toggle_task: Option<Task<()>>,
    cli_status_task: Option<Task<()>>,
    manage_cli_task: Option<Task<()>>,
}

impl EventEmitter<HarnessesEvent> for HarnessesPage {}

impl HarnessesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            harnesses: Loadable::Idle,
            target_device: None,
            target_generation: 0,
            device_menu_open: false,
            device_menu_pressed_open: false,
            error: None,
            cli_status: HashMap::new(),
            cli_update_prompt: None,
            announced_cli_updates: HashSet::new(),
            connection_status: None,
            checking_updates: false,
            managing_cli: None,
            load_task: None,
            toggle_task: None,
            cli_status_task: None,
            manage_cli_task: None,
        };
        page.load(cx);
        page
    }

    /// Params with the `targetDeviceId` passthrough merged in. Inside a paired
    /// device the pick starts from that machine, not from an empty pick — an
    /// untargeted harness call there would describe the controller.
    fn with_target(&self, mut value: serde_json::Value, cx: &App) -> serde_json::Value {
        let target = self
            .target_device
            .clone()
            .or_else(|| self.state.read(cx).harness_target_device());
        if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// Retarget the page at another device: a different device is a different
    /// install/enablement world, so drop the rows and reload through it.
    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        self.target_generation = self.target_generation.wrapping_add(1);
        self.error = None;
        self.harnesses = Loadable::Idle;
        self.cli_status.clear();
        self.cli_update_prompt = None;
        self.announced_cli_updates.clear();
        self.connection_status = None;
        self.checking_updates = false;
        self.managing_cli = None;
        self.load(cx);
        cx.notify();
    }

    /// `ListHarnesses` against the target device (installed probe + enabled
    /// set both come from where the CLIs actually live).
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}), cx);
        let generation = self.target_generation;
        self.harnesses = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LIST_HARNESSES, params).await;
            this.update(cx, |page, cx| {
                if page.target_generation != generation {
                    return;
                }
                page.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if matches!(page.harnesses, Loadable::Ready(_)) {
                    page.load_cli_status(cx, true, false);
                    page.load_connection_status(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_connection_status(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "usageMode": "none" }), cx);
        let generation = self.target_generation;
        self.connection_status = None;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await
                .and_then(|value| {
                    serde_json::from_value::<AgentAccountsSnapshot>(value)
                        .map_err(|error| komet_rpc::RpcError::Failed(error.to_string()))
                });
            this.update(cx, |page, cx| {
                if page.target_generation != generation {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        page.connection_status = Some(snapshot.into());
                    }
                    Err(error) => {
                        page.connection_status = None;
                        page.error = Some(format!("Could not check connected agents: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

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
                        Err(err) => page.error = Some(err.to_string()),
                    },
                    Err(err) => page.error = Some(err.to_string()),
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
        let version_copy = match (&status.installed_version, &status.latest_version) {
            (Some(installed), Some(latest)) => {
                format!("Version {installed} is installed; version {latest} is available.")
            }
            _ => "A newer version is available.".to_string(),
        };
        let card = crate::popover::dialog_card(&Theme::of(cx).clone())
            .child(crate::popover::dialog_title(
                &Theme::of(cx).clone(),
                &format!("{name} update available"),
            ))
            .child(div().mt(px(8.0)).child(crate::popover::dialog_body(
                &Theme::of(cx).clone(),
                format!("{version_copy} New models often need the latest version."),
            )))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        crate::popover::btn_ghost(
                            &Theme::of(cx).clone(),
                            "Later",
                            "cli-update-later",
                            cx.entity_id(),
                        )
                        .id("cli-update-later")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cli_update_prompt = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        crate::popover::btn_primary(&Theme::of(cx).clone(), "Update")
                            .id("cli-update-now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cli_update_prompt = None;
                                this.manage_cli(harness, cx);
                            })),
                    ),
            )
            .into_any_element();
        Some(crate::popover::modal(
            "harness-cli-update-dialog",
            viewport,
            card,
        ))
    }

    /// Flip one harness on the target device. The reply carries the device's
    /// fresh catalog, so the rows repaint from the authoritative state in one
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
                    Err(err) => page.error = Some(err.to_string()),
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
                        Err(err) => page.error = Some(err.to_string()),
                    },
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The page-header device switcher (the Accounts pattern): platform glyph
    /// · name · presence dot · sort glyph, opening a dropdown of every
    /// registered device.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
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
        let open = self.device_menu_open;

        let mut trigger =
            div()
                .id("harnesses-device-switcher")
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
                    cx.listener(|this, _, _, _| {
                        this.device_menu_pressed_open = this.device_menu_open;
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    // A press that found the menu open closes it — never
                    // reopen on the same gesture.
                    let pressed_open = std::mem::take(&mut this.device_menu_pressed_open);
                    this.device_menu_open = !pressed_open && !this.device_menu_open;
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

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
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
                        format!("harnesses-device-row-{ix}"),
                        cx.entity_id(),
                    )
                    .id(("harnesses-device-row", ix))
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
            trigger = trigger.child(popover::anchored_menu("harnesses-device-menu", menu, None));
        }
        trigger.into_any_element()
    }

    fn rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = Theme::of(cx).clone();
        let Loadable::Ready(list) = &self.harnesses else {
            return Vec::new();
        };
        let descriptors = visible_harnesses(list);
        let enabled_count = descriptors.iter().filter(|d| descriptor_enabled(d)).count();
        descriptors
            .into_iter()
            .enumerate()
            .map(|(ix, descriptor)| {
                let harness = descriptor.id;
                let installed = descriptor.installed;
                let enabled = descriptor_enabled(&descriptor);
                let cli_status = self.cli_status.get(&harness);
                let update_available = cli_status.is_some_and(|status| status.update_available);
                let show_manage_button = cli_status.is_some_and(|status| {
                    status.managed
                        && (!installed
                            || status.update_available
                            || status.latest_version.is_none())
                });
                let managing = self.managing_cli == Some(harness);
                // The one enabled harness left can't be switched off — the
                // composer needs something to run (mirrors the engine guard).
                let last_enabled = enabled && enabled_count == 1;
                let interactive = installed && !last_enabled;
                let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
                let mut meta: Vec<gpui::AnyElement> = vec![
                    div()
                        .child(SharedString::from(blurb(harness)))
                        .into_any_element(),
                ];
                // One compact capability line, not five: the row stays scannable
                // while every declared absence is still spelled out.
                let phrases = surface_phrases(&descriptor.surface);
                if !phrases.is_empty() {
                    meta.push(
                        div()
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(SharedString::from(phrases.join("  ·  ")))
                            .into_any_element(),
                    );
                }
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
                // widgets::row_tile with the brand tint honored (the Claude
                // mark keeps its orange, like the picker rail).
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
                widgets::card_row(&theme, ix == 0)
                    .id(("harness-row", ix))
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
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div().flex_1().min_w_0().child(widgets::row_title(
                                            &theme,
                                            descriptor.name.clone(),
                                        )),
                                    )
                                    .when(show_manage_button, |el| {
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
                                        el.child(
                                            widgets::ghost_action(&theme)
                                                .id(("harness-cli-manage", ix))
                                                .hover(|s| widgets::ghost_hover(&theme, s))
                                                .when(!managing, |el| {
                                                    el.on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            this.manage_cli(harness, cx);
                                                        },
                                                    )                                                    )
                                                })
                                                .when(show_connect_button(
                                                    harness,
                                                    self.connection_status.as_ref(),
                                                ), |el| {
                                                    el.child(
                                                        widgets::ghost_action(&theme)
                                                            .id(("harness-connect", ix))
                                                            .hover(|s| widgets::ghost_hover(&theme, s))
                                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                                cx.emit(HarnessesEvent::Connect(harness));
                                                            }))
                                                            .child(
                                                                crate::icons::icon(
                                                                    crate::icons::KEY_MINIMALISTIC,
                                                                )
                                                                .size(px(14.0))
                                                                .text_color(theme.text_muted),
                                                            )
                                                            .child(SharedString::from("Connect")),
                                                    )
                                                })
                                                .child(
                                                    crate::icons::icon(action_icon)
                                                        .size(px(14.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child(SharedString::from(label)),
                                        )
                                    }),
                            )
                            .child(widgets::meta_line(&theme, meta)),
                    )
                    .child(
                        widgets::toggle_switch(&theme, enabled)
                            .id(("harness-toggle", ix))
                            .when(interactive, |el| {
                                el.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle(harness, !enabled, cx);
                                    }))
                            }),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for HarnessesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let update_dialog = self.render_cli_update_dialog(window.viewport_size(), cx);
        let body: gpui::AnyElement = match &self.harnesses {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "harnesses-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    window,
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                div()
                    .child(widgets::error_strip(&theme, message))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("harnesses-retry")
                            .mt(px(8.0))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.load(cx);
                                cx.notify();
                            }))
                            .child(SharedString::from("Retry")),
                    )
                    .into_any_element()
            }
            Loadable::Ready(_) => {
                let rows = self.rows(cx);
                widgets::section_card(&theme)
                    .children(rows)
                    .into_any_element()
            }
        };
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());
        let switcher = self.render_device_switcher(&theme, cx);

        div()
            .id("harnesses-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(widgets::page_header(&theme, "Agents", None))
                            .child(switcher),
                    )
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose which coding agents the composer offers. The setting is per \
                             device — switch devices in the header. Install or update supported \
                             CLIs from their cards. Version checks run in the background when \
                             this page opens and can also be started manually.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("harnesses-check-updates")
                            .mt(px(12.0))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .when(!self.checking_updates, |el| {
                                el.on_click(cx.listener(|page, _, _, cx| page.check_updates(cx)))
                            })
                            .child(crate::icons::icon(crate::icons::REFRESH).size(px(14.0)))
                            .child(if self.checking_updates {
                                "Checking for updates…"
                            } else {
                                "Check for updates"
                            }),
                    )
                    .children(error)
                    .child(body),
            )
            .when_some(update_dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_capability_is_spoken_including_the_absences() {
        for harness in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Cursor,
            HarnessId::Devin,
            HarnessId::Grok,
            HarnessId::Hermes,
            HarnessId::Pi,
            HarnessId::Opencode,
            HarnessId::Cline,
            HarnessId::Qoder,
            HarnessId::Antigravity,
            HarnessId::Muse,
            HarnessId::Copilot,
            HarnessId::Mock,
        ] {
            let phrases =
                surface_phrases(&komet_harness::capabilities::interaction_surface(harness));
            assert_eq!(
                phrases.len(),
                5,
                "{}'s row must answer all five capability questions",
                harness.as_str()
            );
            assert!(
                phrases.iter().all(|p| !p.is_empty()),
                "{}'s row has an empty capability phrase",
                harness.as_str()
            );
        }
        // An absent signal is said as an absence, never left implicit.
        let cursor = surface_phrases(&komet_harness::capabilities::interaction_surface(
            HarnessId::Cursor,
        ));
        assert_eq!(cursor[0], "Questions: not supported");
        assert_eq!(cursor[4], "Tool output: result only");
    }

    #[test]
    fn an_unknown_surface_prints_no_capability_claim() {
        // A catalog from an engine predating the field decodes to all-absent,
        // which is not the same statement as "this agent supports nothing".
        assert!(surface_phrases(&HarnessInteractionSurface::default()).is_empty());
    }

    #[test]
    fn connect_is_only_offered_for_unconnected_app_managed_accounts() {
        let status = AgentConnectionStatus {
            connected: HashSet::from([HarnessId::ClaudeCode]),
            unknown: HashSet::from([HarnessId::Antigravity]),
        };

        assert!(!show_connect_button(HarnessId::ClaudeCode, Some(&status)));
        assert!(show_connect_button(HarnessId::Codex, Some(&status)));
        assert!(!show_connect_button(HarnessId::Grok, Some(&status)));
        assert!(!show_connect_button(HarnessId::Antigravity, Some(&status)));
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
}
