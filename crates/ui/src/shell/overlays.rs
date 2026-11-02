use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, Context, IntoElement, MouseButton, Pixels, SharedString, Window, div, prelude::*,
    px,
};
use gpui_tokio::Tokio;
use komet_engine::harness_cli::HarnessCliStatus;
use komet_engine::registry::{HarnessDescriptor, descriptor_enabled};
use komet_proto::HarnessId;
use komet_rpc::methods;

use super::*;
use crate::motion;
use crate::popover;
use crate::state::{ConnectionStatus, EngineHandle};
use crate::theme::Theme;

/// In-app update lifecycle (macOS bundle installs; see `render_update_strip`).
pub enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in — one click restarts into it.
    Ready(PathBuf),
    Failed(SharedString),
}

pub fn newly_detected_update(
    observed_version: Option<&str>,
    status: &komet_update::UpdateStatus,
) -> Option<String> {
    status
        .update_available
        .then_some(status.latest_version.as_deref())
        .flatten()
        .filter(|version| Some(*version) != observed_version)
        .map(str::to_owned)
}

pub fn update_notification_allowed(
    enabled: bool,
    background_only: bool,
    app_focused: bool,
) -> bool {
    enabled && !(background_only && app_focused)
}

pub fn update_modal_should_show(
    version: &str,
    dismissed_version: Option<&str>,
    status: &komet_update::UpdateStatus,
) -> bool {
    status.update_available
        && status.latest_version.as_deref() == Some(version)
        && dismissed_version != Some(version)
}

/// Launch-check results → the rows the home card shows. A CLI that is already
/// current, disabled on this device, or unmanaged never gets a row (the card
/// is an upsell, not a status report).
pub fn harness_updates(
    statuses: Vec<HarnessCliStatus>,
    enabled: Option<&HashSet<HarnessId>>,
) -> Vec<HarnessCliStatus> {
    statuses
        .into_iter()
        .filter(|status| {
            status.managed
                && status.update_available
                && enabled.is_none_or(|enabled| enabled.contains(&status.harness))
        })
        .collect()
}

/// A failed version lookup yields no update row; the engine retries it on its
/// own cadence, and the shell must poll again for that later result to show.
pub fn harness_check_needs_retry(statuses: &[HarnessCliStatus]) -> bool {
    statuses.iter().any(|status| status.check_error.is_some())
}

/// Shell-side poll cadence while a lookup is failing: 2 min doubling to 10 min.
pub const HARNESS_RETRY_FIRST: Duration = Duration::from_secs(2 * 60);
pub const HARNESS_RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// Per-row lifecycle in the home card. A finished row stays visible until the
/// user dismisses the card or a later check drops its harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliUpdateRow {
    Idle,
    Updating,
    Updated(Option<String>),
    Failed(SharedString),
}

/// Whether the row still waits for an update: never started, or failed and
/// retryable. An unseen harness is idle.
pub fn cli_row_pending(rows: &HashMap<HarnessId, CliUpdateRow>, harness: HarnessId) -> bool {
    !matches!(
        rows.get(&harness),
        Some(CliUpdateRow::Updating | CliUpdateRow::Updated(_))
    )
}

/// Enabled harnesses on this device. `None` (registry unavailable) means
/// "don't filter" rather than hiding every row.
pub async fn enabled_harness_ids(
    engine: &EngineHandle,
    target: Option<&str>,
) -> Option<HashSet<HarnessId>> {
    let mut params = serde_json::json!({});
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    let value = engine
        .client()
        .call(methods::LIST_HARNESSES, params)
        .await
        .ok()?;
    let descriptors = serde_json::from_value::<Vec<HarnessDescriptor>>(value).ok()?;
    Some(
        descriptors
            .into_iter()
            .filter(descriptor_enabled)
            .map(|descriptor| descriptor.id)
            .collect(),
    )
}

/// Update one CLI, then re-read the engine's just-refreshed status. A clean
/// updater exit is not success: the version must actually have moved.
pub async fn run_harness_update(
    engine: &EngineHandle,
    harness: HarnessId,
    target: Option<&str>,
) -> Result<Option<String>, String> {
    let mut params = serde_json::json!({ "harness": harness });
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    engine
        .client()
        .call(methods::MANAGE_HARNESS_CLI, params)
        .await
        .map_err(|error| format!("Could not update: {error}"))?;
    let mut params = serde_json::json!({ "checkLatest": false });
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    let value = engine
        .client()
        .call(methods::GET_HARNESS_CLI_STATUS, params)
        .await
        .map_err(|error| format!("Could not verify the update: {error}"))?;
    let statuses: Vec<HarnessCliStatus> = serde_json::from_value(value)
        .map_err(|error| format!("Could not read the update result: {error}"))?;
    let status = statuses
        .into_iter()
        .find(|status| status.harness == harness)
        .ok_or_else(|| "The update finished but the CLI status is unavailable.".to_string())?;
    if status.update_available {
        return Err(format!(
            "Still on {} after updating to {}.",
            status
                .installed_version
                .as_deref()
                .map(version_token)
                .unwrap_or("an unknown version"),
            status
                .latest_version
                .as_deref()
                .unwrap_or("the latest version"),
        ));
    }
    Ok(status.installed_version)
}

/// Row copy: `2.1.309 → 2.1.310`, or only the target when the installed
/// version could not be read. Only the token the check actually compared is
/// shown: `claude --version` answers `2.1.309 (Claude Code)`.
pub fn cli_update_versions(status: &HarnessCliStatus) -> String {
    match (&status.installed_version, &status.latest_version) {
        (Some(installed), Some(latest)) => {
            format!("{} → {}", version_token(installed), latest)
        }
        (_, Some(latest)) => format!("→ {latest}"),
        _ => String::new(),
    }
}

/// The first whitespace-separated part that starts with a digit — same rule the
/// engine's `is_newer` compares with.
pub fn version_token(version: &str) -> &str {
    version
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(version)
}

/// Cadence of the banner countdown — the retry it counts down to is never
/// sooner than the first schedule step (500 ms), so a quarter-second is finer
/// than the thing it measures and cheap enough to run only while reconnecting.
pub const RECONNECT_TICK: Duration = Duration::from_millis(250);

/// The reconnect banner's label: `None` draws nothing. The countdown comes
/// from `deadline`, never from the frame's `retry_in_ms` — that number was
/// snapshotted when the attempt was scheduled, so printing it would freeze the
/// strip at the schedule delay while the retry was actually closer (or overdue).
/// Pure so the wording is testable without a window.
pub fn reconnect_banner(
    status: &ConnectionStatus,
    now: chrono::DateTime<Utc>,
    deadline: Option<chrono::DateTime<Utc>>,
) -> Option<(String, bool)> {
    match status {
        ConnectionStatus::Reconnecting {
            attempt, reason, ..
        } => {
            let remaining = deadline
                .map(|due| due.signed_duration_since(now).num_milliseconds().max(0) as u64)
                .unwrap_or(0);
            let mut label = format!(
                "Reconnecting to the engine — attempt {attempt}/{RECONNECT_MAX_ATTEMPTS}, retrying in {}",
                format_ms(remaining)
            );
            if !reason.is_empty() {
                label.push_str(" · ");
                label.push_str(reason);
            }
            Some((label, true))
        }
        // Degraded: the link carries the app fine, the run it holds does not —
        // nothing is queued and no retry is pending, so just say what.
        ConnectionStatus::Degraded { reason } => Some((reason.clone(), false)),
        _ => None,
    }
}

impl Shell {
    /// Run the CLI's own updater, then verify the version the binary reports.
    /// Failures stay on the row instead of a modal.
    pub(super) fn update_harness_cli(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if matches!(
            self.cli_update_rows.get(&harness),
            Some(CliUpdateRow::Updating)
        ) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.cli_update_rows.insert(harness, CliUpdateRow::Updating);
        cx.notify();
        let target = self.state.read(cx).harness_target_device();
        cx.spawn(async move |this, cx| {
            let outcome = run_harness_update(&engine, harness, target.as_deref()).await;
            this.update(cx, |shell, cx| {
                shell.finish_harness_update(harness, outcome, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Sequential on purpose: concurrent npm installs fight over the same
    /// global lock.
    pub(super) fn update_all_harness_clis(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<HarnessId> = self
            .cli_updates
            .iter()
            .map(|status| status.harness)
            .filter(|harness| cli_row_pending(&self.cli_update_rows, *harness))
            .collect();
        if targets.len() < 2 {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.state.read(cx).harness_target_device();
        // Mark every target before spawning so none can be started again from
        // its row while the queue is still working through earlier harnesses.
        for harness in &targets {
            self.cli_update_rows
                .insert(*harness, CliUpdateRow::Updating);
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            for harness in targets {
                let outcome = run_harness_update(&engine, harness, target.as_deref()).await;
                let done = this.update(cx, |shell, cx| {
                    shell.finish_harness_update(harness, outcome, cx);
                    cx.notify();
                });
                if done.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    pub(super) fn finish_harness_update(
        &mut self,
        harness: HarnessId,
        outcome: Result<Option<String>, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(version) => {
                self.cli_update_rows
                    .insert(harness, CliUpdateRow::Updated(version));
                // The new version may ship new models; every picker refetches.
                crate::pickers::bump_harness_catalog(cx);
            }
            Err(error) => {
                self.cli_update_rows
                    .insert(harness, CliUpdateRow::Failed(error.into()));
            }
        }
    }

    /// Floating notification for agent CLI updates. It remains visible across
    /// routes and active sessions, and stays dismissed after × for this run.
    pub(super) fn render_harness_updates_card(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.cli_updates.is_empty() || self.cli_updates_dismissed {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let busy = self.cli_updates.iter().any(|status| {
            matches!(
                self.cli_update_rows.get(&status.harness),
                Some(CliUpdateRow::Updating)
            )
        });
        let pending = self
            .cli_updates
            .iter()
            .filter(|status| cli_row_pending(&self.cli_update_rows, status.harness))
            .count();
        let any_updated = self.cli_updates.iter().any(|status| {
            matches!(
                self.cli_update_rows.get(&status.harness),
                Some(CliUpdateRow::Updated(_))
            )
        });
        let title = if self.cli_updates.len() == 1 {
            "Harness update available"
        } else {
            "Harness updates available"
        };

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            );
        if pending > 1 {
            header = header.child(
                div()
                    .id("harness-updates-update-all")
                    .flex_none()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.7))
                    .child(SharedString::from("Update all"))
                    .on_click(cx.listener(|this, _, _, cx| this.update_all_harness_clis(cx))),
            );
        }
        header = header.child(
            div()
                .id("harness-updates-dismiss")
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.0))
                .rounded(px(6.0))
                .when(busy, |el| el.opacity(0.4).cursor_default())
                .when(!busy, |el| {
                    el.cursor_pointer()
                        .hover(|style| style.opacity(0.7))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cli_updates_dismissed = true;
                            cx.notify();
                        }))
                })
                .child(
                    icon(icons::CLOSE)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                ),
        );

        let mut card = popover::popover_card_flush(&theme)
            .w(px(340.0))
            .child(header);
        card = card.child(div().h(px(1.0)).bg(theme.border));
        for status in &self.cli_updates {
            let harness = status.harness;
            let (brand, tint) = crate::pickers::harness_brand_icon(harness);
            let state = self
                .cli_update_rows
                .get(&harness)
                .cloned()
                .unwrap_or(CliUpdateRow::Idle);
            let info: AnyElement = match &state {
                CliUpdateRow::Updated(version) => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.success)
                    .child(icon(icons::CHECK).size(px(12.0)).text_color(theme.success))
                    .child(SharedString::from(match version {
                        Some(version) => format!("Updated to {version}"),
                        None => "Updated".to_string(),
                    }))
                    .into_any_element(),
                _ => div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(cli_update_versions(status)))
                    .into_any_element(),
            };
            let action = match &state {
                CliUpdateRow::Updating => Some(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Updating…"))
                        .into_any_element(),
                ),
                CliUpdateRow::Updated(_) => None,
                CliUpdateRow::Idle | CliUpdateRow::Failed(_) => {
                    let label = if matches!(&state, CliUpdateRow::Failed(_)) {
                        "Retry"
                    } else {
                        "Update"
                    };
                    Some(
                        popover::btn_primary(&theme, label)
                            .id(format!("harness-update-{}", harness.display_name()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update_harness_cli(harness, cx)
                            }))
                            .into_any_element(),
                    )
                }
            };
            let mut row = div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(12.0))
                .py(px(8.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            icon(brand)
                                .size(px(16.0))
                                .text_color(tint.unwrap_or(theme.text_muted)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from(harness.display_name())),
                        )
                        .child(info)
                        .when_some(action, |el, action| el.child(action)),
                );
            if let CliUpdateRow::Failed(error) = &state {
                row = row.child(
                    div()
                        .pl(px(26.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(error.clone()),
                );
            }
            card = card.child(row);
        }
        card = card.child(div().h(px(1.0)).bg(theme.border));
        card = card.child(
            div()
                .px(px(12.0))
                .py(px(8.0))
                .text_size(px(12.0))
                .text_color(theme.text_muted.opacity(0.75))
                .child(SharedString::from(if any_updated {
                    "Model picker refreshed with the new version’s models."
                } else {
                    "New models often need the latest version."
                })),
        );
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .size_0()
                .child(
                    gpui::deferred(
                        gpui::anchored()
                            .anchor(gpui::Anchor::TopRight)
                            .snap_to_window_with_margin(px(12.0))
                            .child(
                                // Only the card occludes: the padding that
                                // pushes it below the titlebar must not swallow
                                // clicks on the window controls up there.
                                div()
                                    .pt(px(Theme::TITLEBAR_HEIGHT + 4.0))
                                    .child(card.occlude()),
                            ),
                    )
                    .priority(1)
                    .into_any_element(),
                )
                .into_any_element(),
        )
    }

    /// Update strip: shown above the user menu whenever the engine's
    /// UpdateStatus stream reports a newer release. On a macOS bundle install
    /// it drives the whole flow — click to download, then click to restart into
    /// the staged bundle. Elsewhere (managed/source installs) it is advisory
    /// (`komet update`); click dismisses it for that version.
    pub(super) fn render_update_strip(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.state.read(cx).update.clone()?;
        if !status.update_available {
            return None;
        }
        let latest = status.latest_version.clone()?;
        if self.update_dismissed.as_deref() == Some(latest.as_str()) {
            return None;
        }
        let mac_app = matches!(self.install, komet_update::InstallKind::MacApp { .. });

        let (label, clickable): (SharedString, bool) = if mac_app {
            match &self.update_flow {
                UpdateFlow::Idle => (format!("Update available — v{latest}").into(), true),
                UpdateFlow::Downloading => (format!("Downloading v{latest}…").into(), false),
                UpdateFlow::Ready(_) => ("Update ready — restart to apply".into(), true),
                UpdateFlow::Failed(message) => (format!("Update failed: {message}").into(), true),
            }
        } else {
            (
                format!("Update available — v{latest} · run `komet update`").into(),
                true,
            )
        };
        let failed = matches!(self.update_flow, UpdateFlow::Failed(_));
        let tone = if failed { theme.danger } else { theme.accent };
        // Dark-purple GLASS tint (user request), not the 400-level accent as
        // a fill: deep pigment at partial alpha tints the blur showing
        // through instead of compositing into the slab that a bright indigo
        // fill produced (earlier user report). Light chrome gets a lavender
        // accent wash instead — dark purple under indigo-600 text goes muddy.
        let (chip_bg, chip_bg_hover) = if failed {
            (theme.danger.opacity(0.14), theme.danger.opacity(0.22))
        } else {
            match theme.appearance {
                crate::theme::Appearance::Dark => {
                    let purple = crate::theme::oklch(0.35, 0.12, 277.0);
                    (purple.opacity(0.45), purple.opacity(0.60))
                }
                crate::theme::Appearance::Light => {
                    (theme.accent.opacity(0.10), theme.accent.opacity(0.16))
                }
            }
        };

        let mut strip = div()
            .id("update-strip")
            .mx(px(Theme::SPACE_SM))
            // No bottom margin: the user-menu block below carries its own
            // SPACE_SM padding — doubling it read as a hole (user report).
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(chip_bg)
            .flex()
            .flex_row()
            .items_center()
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(div().flex_1().min_w_0().child(label));
        if clickable {
            strip = strip
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.on_update_strip_click(cx)));
        }
        Some(strip.into_any_element())
    }

    /// Reconnect banner (transparency plan Task 10): a thin in-flow strip above
    /// the workspace while the link is `Reconnecting` or `Degraded`. It never
    /// hides or replaces the frame — the transcript and composer stay exactly
    /// where they are, the strip just takes its own row — because a lost link
    /// that blanks the screen is indistinguishable from a crashed app.
    /// `link_status()` (not `connection`) is what it speaks, so a paired
    /// environment that went offline reads the same way with no second path.
    pub(super) fn render_reconnect_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let (status, deadline) = {
            let state = self.state.read(cx);
            (state.link_status(), state.reconnect_deadline)
        };
        let reconnecting = matches!(status, ConnectionStatus::Reconnecting { .. });
        let (label, show_retry) = reconnect_banner(&status, Utc::now(), deadline)?;
        let tone = if reconnecting {
            theme.text_muted
        } else {
            theme.warning
        };
        let mut strip = div()
            .id("reconnect-banner")
            .flex_none()
            .w_full()
            .px(px(Theme::SPACE_MD))
            .py(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .bg(tone.opacity(0.08))
            .border_b_1()
            .border_color(tone.opacity(0.20))
            .text_size(px(11.0))
            .text_color(tone)
            .child(div().flex_1().min_w_0().child(SharedString::from(label)));
        if show_retry {
            // Manual retry: `AppState::bootstrap` zeroes the attempt counter and
            // the deadline, so this skips the rest of the backoff chain.
            strip = strip.child(
                div()
                    .id("reconnect-retry-now")
                    .flex_none()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.glass_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                    .child(SharedString::from("Retry now")),
            );
        }
        Some(strip.into_any_element())
    }

    pub(super) fn render_update_modal(
        &mut self,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let version = self.update_modal_version.clone()?;
        let status = self.state.read(cx).update.clone()?;
        if status.latest_version.as_deref() != Some(version.as_str()) || !status.update_available {
            return None;
        }
        let expected_asset = match &self.install {
            komet_update::InstallKind::MacApp { .. } => {
                Some(komet_update::mac_app_artifact(&version))
            }
            komet_update::InstallKind::Managed { .. } => {
                Some(komet_update::headless_artifact(&version))
            }
            komet_update::InstallKind::Unmanaged => None,
        };
        let asset_missing = expected_asset.as_ref().is_some_and(|asset| {
            !status.available_assets.is_empty() && !status.available_assets.contains(asset)
        });
        let kind = self.install.clone();
        let release_url = komet_update::release_url(&version);
        let mut card = popover::dialog_card(theme)
            .child(popover::dialog_title(theme, "Komet update available"))
            .child(div().mt(px(8.0)).child(popover::dialog_body(
                theme,
                format!(
                    "You are running v{}. Version v{} is available.",
                    status.current_version, version
                ),
            )));
        let install_hint = match &self.install {
            komet_update::InstallKind::MacApp { .. } => None,
            komet_update::InstallKind::Managed { .. } => {
                Some("This install is managed; run `komet update` to update it.")
            }
            komet_update::InstallKind::Unmanaged => {
                Some("This install is unmanaged; download a release manually.")
            }
        };
        if let Some(hint) = install_hint {
            card = card.child(div().mt(px(8.0)).text_color(theme.text_muted).child(hint));
        }
        if asset_missing {
            card = card.child(div().mt(px(8.0)).text_color(theme.danger).child(format!(
                "No compatible {} asset was published.",
                expected_asset.as_deref().unwrap_or("platform")
            )));
        } else if let Some(expected_asset) = expected_asset.as_deref()
            && !status.available_assets.is_empty()
        {
            card = card.child(
                div()
                    .mt(px(8.0))
                    .text_color(theme.text_muted)
                    .child(format!("Platform asset: {expected_asset}")),
            );
        }
        let action = match kind {
            komet_update::InstallKind::MacApp { .. } if !asset_missing => {
                let label = match self.update_flow {
                    UpdateFlow::Downloading => "Downloading…",
                    UpdateFlow::Ready(_) => "Restart to apply",
                    _ => "Download and restart",
                };
                let enabled = !matches!(self.update_flow, UpdateFlow::Downloading);
                let mut button = popover::btn_primary(theme, label).id("update-modal-action");
                if enabled {
                    button = button.on_click(cx.listener(|this, _, _, cx| {
                        this.on_update_strip_click(cx);
                    }));
                }
                button
            }
            _ => popover::btn_primary(theme, "View release notes")
                .id("update-modal-release")
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&release_url))),
        };
        card = card.child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_ghost(theme, "Later", "update-modal-later", cx.entity_id())
                        .id("update-modal-later")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_modal_version = None;
                            cx.notify();
                        })),
                )
                .child(action),
        );
        Some(popover::modal(
            "update-modal",
            viewport,
            card.into_any_element(),
        ))
    }

    /// Idle → download; Ready → swap + relaunch; Failed → retry; advisory
    /// installs → dismiss for this version.
    pub(super) fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.install, komet_update::InstallKind::MacApp { .. }) {
            self.update_dismissed = self
                .state
                .read(cx)
                .update
                .as_ref()
                .and_then(|s| s.latest_version.clone());
            cx.notify();
            return;
        }
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Idle | UpdateFlow::Failed(_) => self.begin_update_download(cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
        }
    }

    /// Fetch the manifest and stage the new Komet desktop bundle under the data dir
    /// (tokio — reqwest); the strip flips to "restart to apply" when done.
    pub(super) fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            let manifest = komet_update::fetch_latest().await?;
            komet_update::stage_mac_app(&manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_flow = match outcome {
                    Ok(staged) => UpdateFlow::Ready(staged),
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        UpdateFlow::Failed(message.into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    pub(super) fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let komet_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match komet_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                komet_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed(format!("{err:#}").into());
                cx.notify();
            }
        }
    }

    /// Scope-aware sidebar identity and account menu. Local runtimes advertise
    /// their storage boundary and offer sync; synced runtimes offer sign-out.
    pub(super) fn render_user_menu(
        &mut self,
        user_line: SharedString,
        trigger_subline: Option<SharedString>,
        menu_identity: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu.is_open();
        let action = account_menu_action(self.state.read(cx).workspace_scope, self.sync_flow);
        // Bottom-of-sidebar identity: avatar circle + scope/account label and
        // its secondary status line.
        let initial: SharedString = user_line
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            // user-menu.tsx trigger: hover `bg-white/[0.04]`, open state
            // (`data-[state=open]`) the slightly stronger `bg-white/[0.06]`;
            // the hover wash fades over `transition-colors`.
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger", cx.entity_id()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.user_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                // A press that found the menu open closes it (the card's
                // mouse-down-out already began the close) — never reopen.
                if this.user_menu.take_press_was_open() {
                    this.close_user_menu(cx);
                } else {
                    this.user_menu.open(());
                }
                cx.notify();
            }))
            .child(
                // Avatar: white circle, initial in near-black (komet user-menu.tsx).
                div()
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.bg)
                    .child(initial),
            )
            .child(
                // Name with an optional status line underneath — no chip on the right.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .line_height(px(17.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .truncate()
                            .child(user_line.clone()),
                    )
                    .when_some(trigger_subline, |identity, subline| {
                        identity.child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(15.0))
                                .text_color(theme.text_muted)
                                .child(subline),
                        )
                    }),
            );
        if self.user_menu.get().is_some() {
            let closing = self.user_menu.closing_since();
            // user-menu.tsx content: `w-[--radix-dropdown-menu-trigger-width]`
            // (exactly as wide as the trigger row — sidebar minus its p-2
            // gutters), `flex-col gap-0.5`, then: one small muted email line
            // (`px-2 pb-1 pt-1.5 text-[11px] text-muted-foreground/70`),
            // the action selected by the runtime scope, then "Settings".
            let menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_user_menu(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .truncate()
                        .child(menu_identity),
                )
                .when_some(action, |menu, action| {
                    let row = match action {
                        AccountMenuAction::SyncInProgress => popover::menu_row(
                            theme,
                            false,
                            "user-menu-sync-progress",
                            cx.entity_id(),
                        )
                        .id("user-menu-sync-progress")
                        .opacity(0.6)
                        .child(
                            icon(icons::GLOBAL)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sync setup in progress"))
                        .into_any_element(),
                        AccountMenuAction::RestartPending => popover::menu_row(
                            theme,
                            false,
                            "user-menu-sync-restart",
                            cx.entity_id(),
                        )
                        .id("user-menu-sync-restart")
                        .on_click(cx.listener(|this, _, _, cx| this.reopen_sync_notice(cx)))
                        .child(
                            icon(icons::RESTART)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Finish sync setup"))
                        .into_any_element(),
                        AccountMenuAction::SignOut => {
                            popover::menu_row(theme, false, "user-menu-signout", cx.entity_id())
                                .id("user-menu-signout")
                                .on_click(cx.listener(|this, _, _, cx| this.request_sign_out(cx)))
                                .child(
                                    icon(icons::LOGOUT_2)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Sign out"))
                                .into_any_element()
                        }
                    };
                    menu.child(row).child(popover::menu_separator())
                })
                .child(
                    popover::menu_row(theme, false, "user-menu-settings", cx.entity_id())
                        .id("user-menu-settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsSection::Devices, cx)
                        }))
                        .child(
                            icon(icons::SETTINGS_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Settings")),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_above(
                "user-menu-popover",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    /// Floating layers owned by the shell: context menus, edit dialogs, and
    /// the local-to-synced account lifecycle.
    pub(super) fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((chat_id, position)) = self.chat_menu.get().cloned() {
            let chat_menu_closing = self.chat_menu.closing_since();
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let pin_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let pin_label = {
                let pinned = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .is_some_and(|c| c.pinned);
                if pinned { "Unpin" } else { "Pin" }
            };
            let menu =
                popover::popover_card(&theme)
                    .w(px(170.0))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.close_chat_menu(cx);
                    }))
                    .flex()
                    .flex_col()
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-rename-{chat_id}"),
                            cx.entity_id(),
                        )
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                    )
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-pin-{chat_id}"),
                            cx.entity_id(),
                        )
                        .id("chat-menu-pin")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_chat_pinned(pin_id.clone(), cx)
                        }))
                        .child(icon(icons::PIN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from(pin_label)),
                    )
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-archive-{chat_id}"),
                            cx.entity_id(),
                        )
                        .id("chat-menu-archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archive_chat(archive_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::ARCHIVE_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Archive")),
                    )
                    .child(popover::menu_separator())
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-delete-{chat_id}"),
                            cx.entity_id(),
                        )
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_chat_menu(cx);
                            this.delete_confirm = Some(delete_id.clone());
                            this.delete_focus_pending = true;
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Delete…")),
                    )
                    .into_any_element();
            overlays.push(popover::menu_at(
                "chat-context-menu",
                position,
                menu,
                chat_menu_closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
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
                                "rename-chat-cancel",
                                cx.entity_id(),
                            )
                            .id("rename-chat-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_dialog = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            if std::mem::take(&mut self.delete_focus_pending) {
                window.focus(&self.delete_focus, cx);
            }
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .track_focus(&self.delete_focus)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.delete_confirm = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel", cx.entity_id())
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        if let Some(sync) = self.render_sync_overlay(viewport, window, cx) {
            overlays.push(sync);
        }
        if let Some(update) = self.render_update_modal(viewport, &theme, cx) {
            overlays.push(update);
        }
        if let Some(card) = self.render_harness_updates_card(cx) {
            overlays.push(card);
        }

        overlays
    }
}
