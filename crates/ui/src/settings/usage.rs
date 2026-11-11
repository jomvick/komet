//! Settings → Usage: provider quota windows next to Komet-measured token
//! totals (issue #53, M1–M2).
//!
//! Everything here reads a snapshot first: the page renders the last known
//! values immediately and only probes the agents the user selected, when the
//! page is actually shown. There is no timer and no polling — the engine
//! enforces the probe budget (60 s display cache, 30 s manual-refresh cooldown)
//! and a hidden page never asks for anything.
//!
//! Provider quotas and Komet-measured usage are rendered as separate blocks on
//! purpose: measured tokens are not provider billing, and a missing provider
//! meter is "not reported", never zero.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, SharedString, Task, Window, div, prelude::*, px,
    relative,
};

use komet_proto::{
    AgentUsageReport, AgentUsageWindow, HarnessId, UsageQuotaPoint, UsageReport, UsageTotals,
};
use komet_rpc::methods;

use crate::popover::Loadable;
use crate::settings::providers::{PROVIDERS, harness_display_name, supports_in_app_login};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Agents whose providers expose a quota endpoint today (M3 keeps their real
/// meter shapes; the rest are measured-usage-only).
const QUOTA_CAPABLE: [HarnessId; 5] = [
    HarnessId::ClaudeCode,
    HarnessId::Codex,
    HarnessId::Cursor,
    HarnessId::Antigravity,
    HarnessId::Copilot,
];

/// A stale visible page may refresh itself, but not more often than this.
const AUTO_REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);
/// Minimum sample span before a time-to-exhaustion estimate is meaningful.
const MIN_PROJECTION_SPAN: f64 = 10.0 / 60.0; // hours

pub enum UsageEvent {
    /// The user added or removed an agent; the shell persists the list.
    SelectionChanged(Vec<HarnessId>),
}

pub struct UsagePage {
    state: Entity<AppState>,
    report: Loadable<UsageReport>,
    selected: Vec<HarnessId>,
    loading: bool,
    generation: u64,
    task: Option<Task<()>>,
    picker_open: bool,
    last_load: Option<Instant>,
    error: Option<SharedString>,
}

impl EventEmitter<UsageEvent> for UsagePage {}

impl UsagePage {
    pub fn new(state: Entity<AppState>, selected: Vec<HarnessId>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            report: Loadable::Idle,
            selected,
            loading: false,
            generation: 0,
            task: None,
            picker_open: false,
            last_load: None,
            error: None,
        };
        cx.observe(&page.state, |page, state, cx| {
            if page.report.ready().is_none() && state.read(cx).engine().is_some() {
                page.load("cached", cx);
            }
        })
        .detach();
        page.load("cached", cx);
        page
    }

    /// Called by the shell whenever the Usage route is rendered. Reloads only
    /// when the last load is stale, so a re-render never becomes a probe.
    pub fn on_shown(&mut self, cx: &mut Context<Self>) {
        if self
            .last_load
            .is_none_or(|at| at.elapsed() >= AUTO_REFRESH_AFTER)
        {
            self.load("cached", cx);
        }
    }

    /// `mode`: `"cached"` (display open) or `"fresh"` (manual Refresh).
    ///
    /// Opening is two-phase: `usageMode: "none"` paints the persisted samples
    /// immediately (no network), then the `cached` probe updates the same
    /// cards if the provider answers. The page is never blank waiting on HTTP.
    fn load(&mut self, mode: &'static str, cx: &mut Context<Self>) {
        self.last_load = Some(Instant::now());
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if !matches!(self.report, Loadable::Ready(_)) {
                self.report = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        if matches!(self.report, Loadable::Idle) {
            self.report = Loadable::Loading;
        }
        self.loading = true;
        let target = self.state.read(cx).harness_target_device();
        let selected = self.selected.clone();
        let params = |mode: &str| {
            let mut value = serde_json::json!({
                "agents": selected,
                "usageMode": mode,
            });
            if let (Some(target), Some(object)) = (target.clone(), value.as_object_mut()) {
                object.insert("targetDeviceId".into(), serde_json::json!(target));
            }
            value
        };
        let persisted = (mode == "cached").then(|| params("none"));
        let params = params(mode);
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(persisted) = persisted {
                let result = engine
                    .client()
                    .call(methods::GET_USAGE_REPORT, persisted)
                    .await;
                this.update(cx, |page, cx| {
                    if page.generation != generation {
                        return;
                    }
                    page.apply(result);
                    cx.notify();
                })
                .ok();
            }
            let result = engine
                .client()
                .call(methods::GET_USAGE_REPORT, params)
                .await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.loading = false;
                page.apply(result);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply(&mut self, result: Result<serde_json::Value, komet_rpc::RpcError>) {
        let previous = self.report.ready().cloned();
        self.report = match result.map_err(|err| err.to_string()).and_then(|value| {
            serde_json::from_value::<UsageReport>(value).map_err(|err| err.to_string())
        }) {
            Ok(report) => {
                self.error = None;
                Loadable::Ready(report)
            }
            Err(message) if previous.is_some() => {
                self.error = Some(SharedString::from(format!("Refresh failed: {message}")));
                Loadable::Ready(previous.expect("checked above"))
            }
            Err(message) => {
                self.error = Some(SharedString::from(message.clone()));
                Loadable::Error(message)
            }
        };
    }

    fn set_selected(&mut self, selected: Vec<HarnessId>, cx: &mut Context<Self>) {
        self.selected = selected;
        cx.emit(UsageEvent::SelectionChanged(self.selected.clone()));
        self.load("cached", cx);
    }

    fn add_agent(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.selected.contains(&harness) {
            return;
        }
        self.picker_open = false;
        let mut selected = self.selected.clone();
        selected.push(harness);
        self.set_selected(selected, cx);
    }

    fn remove_agent(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let selected: Vec<HarnessId> = self
            .selected
            .iter()
            .copied()
            .filter(|candidate| *candidate != harness)
            .collect();
        self.set_selected(selected, cx);
    }

    fn report_for(&self, harness: HarnessId) -> Option<&AgentUsageReport> {
        self.report
            .ready()
            .and_then(|report| report.agents.iter().find(|agent| agent.harness == harness))
    }

    fn agent_card(
        &self,
        harness: HarnessId,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let report = self.report_for(harness);
        let account = report.and_then(|report| report.account.as_ref());
        let windows = account
            .map(|account| account.usage_windows.as_slice())
            .unwrap_or(&[]);
        let previous = report.and_then(|report| report.previous_quota.as_ref());
        let mut body = div()
            .px(px(20.0))
            .pb(px(16.0))
            .flex()
            .flex_col()
            .gap(px(12.0));

        if windows.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(if account.is_some() {
                        "Provider quota not reported."
                    } else {
                        "Provider quota not reported — no account signed in on this device."
                    })),
            );
        }
        for window in windows {
            body = body.child(self.quota_row(window, previous, theme, now, cx));
        }

        if let Some(report) = report {
            body = body.child(self.measured_block(report, theme, now));
        }

        let mut card = widgets::section_card(theme);
        let row = widgets::card_row(theme, true)
            .child(widgets::row_tile(theme, crate::icons::SORT_VERTICAL))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .flex_1()
                    .child(widgets::row_title(theme, harness_display_name(harness)))
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            SharedString::from(
                                account
                                    .and_then(|account| account.email.clone())
                                    .unwrap_or_else(|| "No account on this device".into()),
                            )
                            .into_any_element(),
                            SharedString::from(updated_phrase(
                                report.and_then(|report| report.last_quota_at),
                                now,
                            ))
                            .into_any_element(),
                        ],
                    )),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!(
                        "usage-refresh-{}",
                        harness.as_str()
                    )))
                    .flex_none()
                    .text_size(px(12.0))
                    .when(!self.loading, |el| {
                        el.hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(|this, _, _, cx| this.load("fresh", cx)))
                    })
                    .child(crate::icons::icon(crate::icons::REFRESH).size(px(13.0)))
                    .child(SharedString::from(if self.loading {
                        "Refreshing…"
                    } else {
                        "Refresh"
                    })),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!(
                        "usage-remove-{}",
                        harness.as_str()
                    )))
                    .flex_none()
                    .hover(|s| widgets::ghost_hover(theme, s))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_agent(harness, cx)))
                    .child(SharedString::from("Remove")),
            );
        card = card.child(row).child(body);
        card.into_any_element()
    }

    fn quota_row(
        &self,
        window: &AgentUsageWindow,
        previous: Option<&UsageQuotaPoint>,
        theme: &Theme,
        now: DateTime<Utc>,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let fill = if fraction >= 0.9 {
            theme.danger
        } else if fraction >= 0.7 {
            theme.warning
        } else {
            theme.text
        };
        let value = window
            .display_value
            .clone()
            .unwrap_or_else(|| format!("{}%", (fraction * 100.0).round() as i64));
        let mut meta = vec![
            SharedString::from(reset_phrase(window.resets_at, now)).into_any_element(),
            SharedString::from(window_phrase(window.window_seconds)).into_any_element(),
            SharedString::from("Provider").into_any_element(),
        ];
        if let Some(pace) = pace(window, now) {
            let delta = (pace.delta * 100.0).round() as i64;
            meta.push(
                SharedString::from(match delta {
                    d if d > 1 => format!("+{d} pts ahead of pace"),
                    d if d < -1 => format!("{d} pts behind pace"),
                    _ => "On pace".to_string(),
                })
                .into_any_element(),
            );
        }
        if let Some(hours) = time_to_exhaustion(window, previous_sample_for(window, previous), now)
        {
            meta.push(SharedString::from(eta_phrase(hours)).into_any_element());
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(window.label.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(value)),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(6.0))
                    .rounded_full()
                    .bg(theme.border)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(relative(fraction.max(0.01)))
                            .rounded_full()
                            .bg(fill),
                    ),
            )
            .child(widgets::meta_line(theme, meta))
            .into_any_element()
    }

    fn measured_block(
        &self,
        report: &AgentUsageReport,
        theme: &Theme,
        _now: DateTime<Utc>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .pt(px(6.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Komet-measured usage")),
            )
            .child(measured_line(theme, "Last 24 hours", &report.last_day))
            .child(measured_line(theme, "Last 30 days", &report.last_30_days))
            .into_any_element()
    }

    fn picker(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut card = widgets::section_card(theme);
        let mut any = false;
        for (harness, _, _) in PROVIDERS {
            if self.selected.contains(&harness) {
                continue;
            }
            let first = !any;
            any = true;
            card = card.child(
                widgets::card_row(theme, first)
                    .id(SharedString::from(format!(
                        "usage-add-{}",
                        harness.as_str()
                    )))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.03)))
                    .on_click(cx.listener(move |this, _, _, cx| this.add_agent(harness, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .child(widgets::row_title(theme, harness_display_name(harness)))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(capability(harness)).into_any_element(),
                                    SharedString::from(if supports_in_app_login(harness) {
                                        "Signed in via Komet"
                                    } else {
                                        "Signed in via its CLI"
                                    })
                                    .into_any_element(),
                                ],
                            )),
                    )
                    .child(SharedString::from("Add")),
            );
        }
        if !any {
            card = card.child(widgets::card_row(theme, true).child(SharedString::from(
                "Every known agent is already on the page.",
            )));
        }
        card.into_any_element()
    }
}

impl Render for UsagePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let mut page = widgets::page_column()
            .id("usage-page")
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::page_header(
                        &theme,
                        "Usage",
                        Some(self.selected.len()),
                    ))
                    .child(div().flex_1())
                    .child(
                        widgets::ghost_action(&theme)
                            .id("usage-add-agent")
                            .flex_none()
                            .text_size(px(12.5))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.picker_open = !this.picker_open;
                                cx.notify();
                            }))
                            .child(SharedString::from(if self.picker_open {
                                "Close"
                            } else {
                                "+ Agent"
                            })),
                    ),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Provider quotas and Komet-measured tokens. Provider data appears only for the agents you added; measured usage is local to this device.",
            ));

        if let Some(error) = self.error.clone() {
            page = page.child(widgets::error_strip(&theme, error));
        }
        if self.picker_open {
            page = page.child(self.picker(&theme, cx));
        }
        if self.selected.is_empty() {
            page = page.child(widgets::section_card(&theme).child(
                widgets::card_row(&theme, true).child(SharedString::from(
                    "No agents selected. Use “+ Agent” to add one.",
                )),
            ));
        }
        for harness in self.selected.clone() {
            page = page.child(self.agent_card(harness, &theme, now, cx));
        }
        if self.loading && self.report.ready().is_none() {
            page = page.child(
                div()
                    .mt(px(16.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Reading the local usage history…")),
            );
        }
        page
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Provider quota consumed versus the fraction of the window already elapsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    pub elapsed_fraction: f32,
    /// Positive = ahead of pace (burning faster than the window).
    pub delta: f32,
}

/// `None` when the provider gave no window duration or reset time, after the
/// reset, or when the two disagree enough to be clock skew.
pub fn pace(window: &AgentUsageWindow, now: DateTime<Utc>) -> Option<Pace> {
    let resets_at = window.resets_at?;
    let seconds = window.window_seconds? as f32;
    if seconds <= 0.0 {
        return None;
    }
    let remaining = (resets_at - now).num_seconds() as f32;
    if remaining < 0.0 || remaining > seconds {
        return None;
    }
    let elapsed_fraction = (seconds - remaining) / seconds;
    Some(Pace {
        elapsed_fraction,
        delta: window.used_fraction - elapsed_fraction,
    })
}

/// Previous `(at, used_fraction)` for the same window label.
pub fn previous_sample_for<'a>(
    window: &AgentUsageWindow,
    previous: Option<&'a UsageQuotaPoint>,
) -> Option<(&'a DateTime<Utc>, f32)> {
    let point = previous?;
    point
        .windows
        .iter()
        .find(|candidate| candidate.label == window.label)
        .map(|candidate| (&point.at, candidate.used_fraction))
}

/// Hours until the quota is exhausted, from two samples far enough apart.
/// `None` when the history cannot support a projection.
pub fn time_to_exhaustion(
    window: &AgentUsageWindow,
    previous: Option<(&DateTime<Utc>, f32)>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let (at, prior) = previous?;
    if window.used_fraction >= 1.0 || window.used_fraction <= prior {
        return None;
    }
    let hours = (now - *at).num_seconds() as f64 / 3600.0;
    if hours < MIN_PROJECTION_SPAN {
        return None;
    }
    let rate = (window.used_fraction - prior) as f64 / hours;
    if rate <= 0.0 {
        return None;
    }
    Some(((1.0 - f64::from(window.used_fraction)) / rate).max(0.0))
}

fn capability(harness: HarnessId) -> &'static str {
    if QUOTA_CAPABLE.contains(&harness) {
        "Provider quotas + measured usage"
    } else {
        "Measured usage only"
    }
}

fn updated_phrase(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match at {
        Some(at) => format!("Quota updated {}", ago_phrase(now - at)),
        None => "No quota sample yet".to_string(),
    }
}

fn reset_phrase(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match resets_at {
        Some(at) if at <= now => "Reset pending".to_string(),
        Some(at) => format!("Resets in {}", span_phrase((at - now).num_seconds())),
        None => "No reset reported".to_string(),
    }
}

fn window_phrase(seconds: Option<u32>) -> String {
    match seconds {
        Some(seconds) if seconds > 0 => format!("{} window", span_phrase(i64::from(seconds))),
        _ => "Window length not reported".to_string(),
    }
}

fn eta_phrase(hours: f64) -> String {
    if hours >= 48.0 {
        format!("≈ {:.0} days to exhaustion", hours / 24.0)
    } else if hours >= 1.0 {
        format!("≈ {hours:.0} h to exhaustion")
    } else {
        format!("≈ {:.0} min to exhaustion", (hours * 60.0).max(1.0))
    }
}

fn ago_phrase(span: chrono::TimeDelta) -> String {
    let seconds = span.num_seconds().max(0);
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn span_phrase(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", seconds / 60)
    }
}

fn measured_line(theme: &Theme, label: &str, totals: &UsageTotals) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_x(px(10.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .child(
            div()
                .w(px(96.0))
                .text_color(theme.text_muted.opacity(0.75))
                .child(SharedString::from(label.to_string())),
        )
        .child(SharedString::from(format!(
            "{} · {} in · {} cached · {} out · {} reasoning",
            runs_phrase(totals.runs),
            token_phrase(totals.input_tokens),
            token_phrase(totals.cached_input_tokens),
            token_phrase(totals.output_tokens),
            token_phrase(totals.reasoning_tokens),
        )))
        .into_any_element()
}

fn runs_phrase(runs: u64) -> String {
    if runs == 1 {
        "1 run".to_string()
    } else {
        format!("{runs} runs")
    }
}

fn token_phrase(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k tok", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tok")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn window(used: f32, seconds: Option<u32>, resets_in: Option<i64>) -> AgentUsageWindow {
        AgentUsageWindow {
            label: "Session".into(),
            used_fraction: used,
            resets_at: resets_in.map(|s| Utc::now() + TimeDelta::seconds(s)),
            display_value: None,
            window_seconds: seconds,
        }
    }

    #[test]
    fn pace_compares_usage_with_elapsed_window() {
        // Half the window gone, 70% used → +20 pts ahead.
        let ahead = pace(&window(0.7, Some(18_000), Some(9_000)), Utc::now()).unwrap();
        assert!((ahead.elapsed_fraction - 0.5).abs() < 0.01);
        assert!((ahead.delta - 0.2).abs() < 0.01);

        // Quarter gone, 10% used → behind.
        let behind = pace(&window(0.1, Some(18_000), Some(13_500)), Utc::now()).unwrap();
        assert!(behind.delta < 0.0);
    }

    #[test]
    fn pace_is_none_without_duration_reset_or_inside_skew() {
        assert!(pace(&window(0.5, None, Some(9_000)), Utc::now()).is_none());
        assert!(pace(&window(0.5, Some(18_000), None), Utc::now()).is_none());
        // Remaining time longer than the window itself → clock skew.
        assert!(pace(&window(0.5, Some(3_600), Some(7_200)), Utc::now()).is_none());
        // Reset already passed.
        assert!(pace(&window(0.5, Some(18_000), Some(-60)), Utc::now()).is_none());
    }

    #[test]
    fn pace_on_an_exact_window_is_zero() {
        let on_pace = pace(&window(0.5, Some(3_600), Some(1_800)), Utc::now()).unwrap();
        assert!(on_pace.delta.abs() < 0.01);
    }

    #[test]
    fn exhaustion_estimate_needs_a_real_ramp() {
        let now = Utc::now();
        let point = |minutes: i64, used: f32| UsageQuotaPoint {
            at: now - TimeDelta::minutes(minutes),
            windows: vec![window(used, Some(18_000), Some(10_000))],
        };
        let current = window(0.5, Some(18_000), Some(10_000));
        let history = point(60, 0.2);
        let previous = previous_sample_for(&current, Some(&history)).unwrap();
        let hours = time_to_exhaustion(&current, Some(previous), now).unwrap();
        assert!(
            (hours - 1.667).abs() < 0.05,
            "0.3 in 60 min → 0.5 left in 100 min, got {hours}"
        );

        // Too little history.
        let short = point(2, 0.45);
        let previous = previous_sample_for(&current, Some(&short)).unwrap();
        assert!(time_to_exhaustion(&current, Some(previous), now).is_none());
        // Usage did not move (or went down after a reset).
        let flat = point(60, 0.5);
        let previous = previous_sample_for(&current, Some(&flat)).unwrap();
        assert!(time_to_exhaustion(&current, Some(previous), now).is_none());
        // No previous point at all.
        assert!(time_to_exhaustion(&current, None, now).is_none());
    }

    #[test]
    fn previous_sample_matches_by_window_label() {
        let current = window(0.5, Some(18_000), Some(10_000));
        let mut other = window(0.9, Some(604_800), Some(100_000));
        other.label = "Week".into();
        let point = UsageQuotaPoint {
            at: Utc::now() - TimeDelta::minutes(30),
            windows: vec![other],
        };
        assert!(previous_sample_for(&current, Some(&point)).is_none());
    }
}
