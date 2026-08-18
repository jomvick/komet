//! Context & Token Usage telemetry visualization: circular progress ring + detailed popover.
//!
//! Provides real-time visibility into thread token consumption, context window usage,
//! compaction limits, and token breakdowns (input, cached input, output, reasoning).

use gpui::{
    AnyElement, IntoElement, ParentElement, SharedString, Styled, div, hsla, prelude::*, px,
};
use komet_proto::{ContextUsageStats, format_tokens};

use crate::theme::Theme;

/// Render the circular context usage ring indicator (trigger widget).
pub fn render_context_ring(stats: &ContextUsageStats, theme: &Theme) -> AnyElement {
    let ratio = stats.context_ratio();
    let ring_color = if ratio >= 0.90 {
        hsla(0.0 / 360.0, 0.85, 0.55, 1.0)
    } else if ratio >= 0.70 {
        hsla(38.0 / 360.0, 0.92, 0.50, 1.0)
    } else {
        theme.text_muted
    };

    let inner_size = (10.0 * ratio).max(2.0);

    div()
        .id("context-usage-ring")
        .size(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .child(div().size(px(inner_size)).rounded_full().bg(ring_color))
        .into_any_element()
}

/// Render the detailed Context Popover matching the reference UI.
pub fn render_context_popover(stats: &ContextUsageStats, theme: &Theme) -> AnyElement {
    let ratio = stats.context_ratio();
    let percent = stats.context_percent();
    let total_used = stats.total_tokens();

    let fill_color = if ratio >= 0.90 {
        hsla(0.0 / 360.0, 0.85, 0.55, 1.0)
    } else if ratio >= 0.70 {
        hsla(38.0 / 360.0, 0.92, 0.50, 1.0)
    } else {
        theme.text
    };

    let context_header_right = format!(
        "{} / {} · {}%",
        format_tokens(total_used),
        format_tokens(stats.context_limit),
        percent
    );

    let compact_threshold_str = stats
        .compact_threshold
        .map(format_tokens)
        .unwrap_or_else(|| "400k".to_string());

    let compactions_reason_str = stats
        .compactions_reason
        .clone()
        .unwrap_or_else(|| "cache expiry".to_string());

    div()
        .w(px(290.0))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(14.0))
        .text_sm()
        .child(
            // ── Header: Context + limit + progress bar ──────────────────────
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("Context"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text)
                                .child(context_header_right),
                        ),
                )
                .child(
                    // Progress Bar track
                    div()
                        .w_full()
                        .h(px(4.0))
                        .rounded_full()
                        .bg(theme.border)
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(px((262.0 * ratio).max(4.0)))
                                .rounded_full()
                                .bg(fill_color),
                        ),
                )
                .child(
                    // Compacts at row
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .text_xs()
                        .child(div().text_color(theme.text_muted).child("Compacts at"))
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text_muted)
                                .child(compact_threshold_str),
                        ),
                ),
        )
        .child(
            // Subtle horizontal divider
            div().w_full().h(px(1.0)).bg(theme.border),
        )
        .child(
            // ── Thread usage section ───────────────────────────────────────
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("Thread usage"),
                        )
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text)
                                .child(format_tokens(total_used)),
                        ),
                )
                .child(usage_row("Input", format_tokens(stats.input_tokens), theme))
                .child(usage_row(
                    "Cached input (included)",
                    format_tokens(stats.cached_input_tokens),
                    theme,
                ))
                .child(usage_row(
                    "Output",
                    format_tokens(stats.output_tokens),
                    theme,
                ))
                .child(usage_row(
                    "Reasoning output (included)",
                    format_tokens(stats.reasoning_tokens),
                    theme,
                ))
                .child(
                    div()
                        .pt(px(2.0))
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(
                            "Cumulative provider-reported usage in the current thread transcript.",
                        )),
                ),
        )
        .child(
            // Subtle horizontal divider
            div().w_full().h(px(1.0)).bg(theme.border),
        )
        .child(
            // ── Compactions section ────────────────────────────────────────
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("Compactions"),
                        )
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text)
                                .child(stats.compactions_count.to_string()),
                        ),
                )
                .child(usage_row("Latest", compactions_reason_str, theme)),
        )
        .into_any_element()
}

fn usage_row(label: &str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .text_xs()
        .child(
            div()
                .text_color(theme.text_muted)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted)
                .child(SharedString::from(value)),
        )
}
