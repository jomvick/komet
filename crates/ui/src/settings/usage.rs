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
    AgentUsageReport, AgentUsageWindow, HarnessId, UsageBreakdown, UsageCostEstimate, UsageDetail,
    UsagePoint, UsageQuotaPoint, UsageReport, UsageTotals,
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

/// Period the measured block sums. The report carries all three, so switching
/// is free (no probe, no RPC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    const ALL: [Self; 3] = [Self::Day, Self::Week, Self::Month];

    fn label(self) -> &'static str {
        match self {
            Self::Day => "24 h",
            Self::Week => "7 d",
            Self::Month => "30 d",
        }
    }

    fn range_label(self) -> &'static str {
        match self {
            Self::Day => "Last 24 hours",
            Self::Week => "Last 7 days",
            Self::Month => "Last 30 days",
        }
    }

    fn totals(self, report: &AgentUsageReport) -> &UsageTotals {
        match self {
            Self::Day => &report.last_day,
            Self::Week => &report.last_7_days,
            Self::Month => &report.last_30_days,
        }
    }

    fn detail(self, report: &AgentUsageReport) -> &UsageDetail {
        match self {
            Self::Day => &report.day_detail,
            Self::Week => &report.week_detail,
            Self::Month => &report.month_detail,
        }
    }

    /// Same-length window immediately before this period (M6 comparison).
    fn previous(self, report: &AgentUsageReport) -> &UsageTotals {
        match self {
            Self::Day => &report.prev_day,
            Self::Week => &report.prev_7_days,
            Self::Month => &report.prev_30_days,
        }
    }

    /// Rate-limit rejections in this period; `None` = journal predates the
    /// counter, which the UI must not paint as a confident zero.
    fn rate_limits(self, report: &AgentUsageReport) -> Option<u64> {
        match self {
            Self::Day => report.rate_limits.day,
            Self::Week => report.rate_limits.week,
            Self::Month => report.rate_limits.month,
        }
    }

    fn cost(self, report: &AgentUsageReport) -> Option<&UsageCostEstimate> {
        match self {
            Self::Day => report.day_cost.as_ref(),
            Self::Week => report.week_cost.as_ref(),
            Self::Month => report.month_cost.as_ref(),
        }
    }

    /// Pre-aggregated buckets for the mini-chart: hourly for 24 h, daily
    /// beyond (M6).
    fn points(self, report: &AgentUsageReport) -> &[UsagePoint] {
        match self {
            Self::Day => &report.series.hourly,
            Self::Week | Self::Month => &report.series.daily,
        }
    }
}

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
    /// Period the Komet-measured block sums (M6 filter). UI-only: the report
    /// already carries 24 h / 7 d / 30 d, so switching costs no RPC.
    period: Period,
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
            period: Period::Day,
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

    /// Copy the visible measured totals as CSV (metadata only — M6 export).
    fn copy_csv(&mut self, cx: &mut Context<Self>) {
        let Some(report) = self.report.ready() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(csv_report(
            report,
            self.period,
        )));
    }

    fn period_button(&self, period: Period, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let active = self.period == period;
        widgets::ghost_action(theme)
            .id(SharedString::from(format!(
                "usage-period-{}",
                period.label()
            )))
            .flex_none()
            .text_size(px(11.5))
            .when(active, |el| {
                el.bg(crate::theme::ink(0.06)).text_color(theme.text)
            })
            .when(!active, |el| el.hover(|s| widgets::ghost_hover(theme, s)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.period = period;
                cx.notify();
            }))
            .child(SharedString::from(period.label()))
            .into_any_element()
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
        let (mark, tint) = crate::pickers::harness_brand_icon(harness);
        let row = widgets::card_row(theme, true)
            .child(widgets::row_tile_branded(theme, mark, tint))
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
        let pace = pace(window, now);
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
        // A billing report (`display_value`, no denominator) gets no gauge: an
        // empty bar would read as "0% used", which is exactly the lie M3 bans.
        let gauge = window.display_value.is_none() || fraction > 0.0;
        let mut meta = vec![
            SharedString::from(reset_phrase(window.resets_at, now)).into_any_element(),
            SharedString::from(window_phrase(window.window_seconds)).into_any_element(),
            SharedString::from("Provider").into_any_element(),
        ];
        if !gauge {
            meta.push(SharedString::from("No quota denominator").into_any_element());
        }
        if let Some(pace) = pace {
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
        let track = gauge.then(|| {
            let mut track = div()
                .relative()
                .w_full()
                .h(px(8.0))
                .rounded_full()
                .bg(theme.border)
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(relative(fraction.max(0.01)))
                        .rounded_full()
                        .bg(fill),
                );
            if let Some(pace) = pace {
                // The "even burn" marker: where usage would sit if the quota
                // were spent linearly across the window. Fill left of it =
                // ahead of pace.
                track = track.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .bottom(px(0.0))
                        .left(relative(pace.elapsed_fraction.clamp(0.0, 1.0)))
                        .w(px(2.0))
                        .bg(theme.text_muted.opacity(0.55)),
                );
            }
            track
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
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
                            .text_color(if fraction >= 0.7 {
                                fill
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(value)),
                    ),
            )
            .when_some(track, |el, track| el.child(track))
            .child(widgets::meta_line(theme, meta))
            .into_any_element()
    }

    fn measured_block(
        &self,
        report: &AgentUsageReport,
        theme: &Theme,
        _now: DateTime<Utc>,
    ) -> AnyElement {
        let totals = self.period.totals(report);
        let mut block = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .pt(px(10.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Komet-measured usage")),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(self.period.range_label())),
                    ),
            )
            .child(measured_line(theme, "Tokens", totals));
        if let Some(rates) = rates_phrase(totals) {
            block = block.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted.opacity(0.75))
                    .child(SharedString::from(rates)),
            );
        }
        if let Some(line) = sparkline(theme, self.period.points(report)) {
            block = block.child(line);
        }
        if let Some(line) = comparison_line(theme, self.period, totals, report) {
            block = block.child(line);
        }
        if let Some(count) = self.period.rate_limits(report) {
            block = block.child(
                div()
                    .text_size(px(11.5))
                    .text_color(if count > 0 {
                        theme.warning
                    } else {
                        theme.text_muted.opacity(0.75)
                    })
                    .child(SharedString::from(format!(
                        "{count} rate limit{}",
                        if count == 1 { "" } else { "s" }
                    ))),
            );
        }
        let cost_line = match self.period.cost(report) {
            Some(cost) => {
                let mut text = format!("≈ ${:.2} API-equivalent (rates {})", cost.usd, cost.as_of);
                if !cost.unpriced_models.is_empty() {
                    text.push_str(&format!(" · {} unpriced", cost.unpriced_models.len()));
                }
                Some(text)
            }
            // Tokens were measured but no model in the window has a known
            // price — say so instead of implying $0.
            None if totals.input_tokens + totals.output_tokens > 0 => {
                Some("API-equivalent cost: tarif inconnu".to_string())
            }
            None => None,
        };
        if let Some(text) = cost_line {
            block = block.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted.opacity(0.75))
                    .child(SharedString::from(text)),
            );
        }
        let detail = self.period.detail(report);
        for (caption, rows, unknown) in [
            ("Models", &detail.models, "unknown model"),
            ("Projects", &detail.projects, "unknown project"),
        ] {
            if let Some(line) = breakdown_line(theme, caption, rows, unknown) {
                block = block.child(line);
            }
        }
        block.into_any_element()
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
            let (mark, tint) = crate::pickers::harness_brand_icon(harness);
            card = card.child(
                // `card_row` already paints the hover wash — a second `.hover`
                // panics gpui ("hover style already set") and kills the app.
                widgets::card_row(theme, first)
                    .id(SharedString::from(format!(
                        "usage-add-{}",
                        harness.as_str()
                    )))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.add_agent(harness, cx)))
                    .child(widgets::row_tile_branded(theme, mark, tint))
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
                    .children(Period::ALL.map(|period| self.period_button(period, &theme, cx)))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("usage-copy-csv")
                            .flex_none()
                            .text_size(px(12.0))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_csv(cx)))
                            .child(SharedString::from("Copy CSV")),
                    )
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
        div()
            .id("usage-page-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(page)
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
                .w(px(56.0))
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

/// Cache-hit and reasoning shares, when the totals can support them (M6).
fn rates_phrase(totals: &UsageTotals) -> Option<String> {
    if totals.runs == 0 {
        return None;
    }
    let mut parts = Vec::new();
    if totals.input_tokens > 0 {
        parts.push(format!(
            "Cache hit {}%",
            percent(totals.cached_input_tokens, totals.input_tokens)
        ));
    }
    if totals.output_tokens > 0 {
        parts.push(format!(
            "Reasoning {}% of output",
            percent(totals.reasoning_tokens, totals.output_tokens)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Tokens a bucket contributes to the mini-chart: fresh input + output
/// (cached input and reasoning are subsets on most providers).
fn point_tokens(point: &UsagePoint) -> u64 {
    point.totals.input_tokens + point.totals.output_tokens
}

/// Compact bar chart of the pre-aggregated buckets (M6). `None` when every
/// bucket is empty — an all-zero chart is noise, not information.
fn sparkline(theme: &Theme, points: &[UsagePoint]) -> Option<AnyElement> {
    let max = points.iter().map(point_tokens).max()?;
    if max == 0 {
        return None;
    }
    let bars = points
        .iter()
        .map(|point| {
            let height = 4.0f32 + 28.0 * (point_tokens(point) as f32 / max as f32);
            div()
                .flex_1()
                .h(px(height))
                .rounded_sm()
                .bg(if point_tokens(point) == 0 {
                    theme.border
                } else {
                    theme.accent.opacity(0.75)
                })
        })
        .collect::<Vec<_>>();
    Some(
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(2.0))
            .h(px(32.0))
            .children(bars)
            .into_any_element(),
    )
}

/// "▲ 23% vs previous 24 h"-style line. Both windows empty → nothing; a zero
/// previous window is reported as new usage, not as an infinite delta.
fn comparison_line(
    theme: &Theme,
    period: Period,
    totals: &UsageTotals,
    report: &AgentUsageReport,
) -> Option<AnyElement> {
    let current = totals.input_tokens + totals.output_tokens;
    let previous = {
        let p = period.previous(report);
        p.input_tokens + p.output_tokens
    };
    if current == 0 && previous == 0 {
        return None;
    }
    let text = if previous == 0 {
        format!(
            "{} — none in the previous {}",
            token_phrase(current),
            period.label()
        )
    } else {
        let delta = (current as f64 - previous as f64) / previous as f64 * 100.0;
        let arrow = if delta >= 0.0 { "▲" } else { "▼" };
        format!(
            "{} {}% vs previous {}",
            arrow,
            delta.abs().round() as i64,
            period.label()
        )
    };
    Some(
        div()
            .text_size(px(11.5))
            .text_color(theme.text_muted.opacity(0.75))
            .child(SharedString::from(text))
            .into_any_element(),
    )
}

/// One breakdown dimension ("Models" / "Projects"): the leading rows as
/// `label · tokens`, `unknown` standing in for a turn that carried none.
fn breakdown_line(
    theme: &Theme,
    caption: &str,
    rows: &[UsageBreakdown],
    unknown: &str,
) -> Option<AnyElement> {
    if rows.is_empty() {
        return None;
    }
    let entries: Vec<String> = rows
        .iter()
        .take(3)
        .map(|row| {
            let label = row.label.clone().unwrap_or_else(|| unknown.to_string());
            format!(
                "{} · {}",
                label,
                token_phrase(row.totals.input_tokens + row.totals.output_tokens)
            )
        })
        .collect();
    Some(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(10.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted.opacity(0.75))
            .child(
                div()
                    .w(px(56.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(caption.to_string())),
            )
            .child(SharedString::from(entries.join("   ")))
            .into_any_element(),
    )
}

fn percent(part: u64, whole: u64) -> u64 {
    ((part as f64 / whole as f64) * 100.0).round() as u64
}

/// CSV of the visible report: usage metadata only (no message text, prompts,
/// credentials, or provider tokens — M6 export constraint).
fn csv_report(report: &UsageReport, period: Period) -> String {
    let mut csv = String::from(
        "agent,account,period,runs,input_tokens,cached_input_tokens,output_tokens,reasoning_tokens\n",
    );
    for agent in &report.agents {
        let totals = period.totals(agent);
        let account = agent
            .account
            .as_ref()
            .and_then(|account| account.email.as_deref())
            .unwrap_or("");
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            csv_field(harness_display_name(agent.harness)),
            csv_field(account),
            period.label(),
            totals.runs,
            totals.input_tokens,
            totals.cached_input_tokens,
            totals.output_tokens,
            totals.reasoning_tokens,
        ));
    }
    csv
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
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

    fn report(period: UsageTotals, account: Option<&str>) -> AgentUsageReport {
        AgentUsageReport {
            harness: HarnessId::ClaudeCode,
            account: account.map(|email| komet_proto::AgentAccount {
                id: "a".into(),
                harness: HarnessId::ClaudeCode,
                email: Some(email.into()),
                plan_label: Some("Max".into()),
                active: true,
                usage_windows: Vec::new(),
                display_name: None,
                organization: None,
                auth_kind: None,
                switchable: true,
                saved_at: None,
            }),
            last_quota_at: None,
            previous_quota: None,
            last_day: period,
            last_7_days: period,
            last_30_days: period,
            day_detail: UsageDetail::default(),
            week_detail: UsageDetail::default(),
            month_detail: UsageDetail::default(),
            prev_day: UsageTotals::default(),
            prev_7_days: UsageTotals::default(),
            prev_30_days: UsageTotals::default(),
            series: komet_proto::UsageSeries::default(),
            rate_limits: komet_proto::UsageRateLimits::default(),
            day_cost: None,
            week_cost: None,
            month_cost: None,
        }
    }

    #[test]
    fn csv_export_is_metadata_only_and_quotes_commas() {
        let totals = UsageTotals {
            runs: 2,
            input_tokens: 100,
            cached_input_tokens: 40,
            output_tokens: 50,
            reasoning_tokens: 10,
        };
        let report = UsageReport {
            agents: vec![report(totals, Some("me,inc@example.com"))],
        };
        let csv = csv_report(&report, Period::Week);
        let mut lines = csv.lines();
        assert_eq!(
            lines.next(),
            Some(
                "agent,account,period,runs,input_tokens,cached_input_tokens,output_tokens,reasoning_tokens"
            )
        );
        assert_eq!(
            lines.next(),
            Some("Claude Code,\"me,inc@example.com\",7 d,2,100,40,50,10")
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn rates_need_a_denominator() {
        let zero = UsageTotals::default();
        assert!(rates_phrase(&zero).is_none());
        let tokens = UsageTotals {
            runs: 1,
            input_tokens: 100,
            cached_input_tokens: 25,
            output_tokens: 40,
            reasoning_tokens: 10,
        };
        let rates = rates_phrase(&tokens).unwrap();
        assert!(rates.contains("Cache hit 25%"), "{rates}");
        assert!(rates.contains("Reasoning 25% of output"), "{rates}");
    }

    #[test]
    fn breakdown_line_labels_unknown_and_sums_tokens() {
        let theme = Theme::dark();
        let rows = vec![
            UsageBreakdown {
                label: Some("claude-x".into()),
                totals: UsageTotals {
                    runs: 2,
                    input_tokens: 1_000_000,
                    cached_input_tokens: 0,
                    output_tokens: 500_000,
                    reasoning_tokens: 0,
                },
            },
            UsageBreakdown {
                label: None,
                totals: UsageTotals {
                    runs: 1,
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_tokens: 0,
                },
            },
        ];
        let element = breakdown_line(&theme, "Models", &rows, "unknown model")
            .expect("non-empty breakdown renders");
        let _ = element;
        assert!(breakdown_line(&theme, "Models", &[], "unknown model").is_none());
    }
}
