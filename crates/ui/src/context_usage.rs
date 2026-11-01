//! Context & Token Usage telemetry visualization: circular progress ring +
//! a dedicated popover.
//!
//! Provides real-time visibility into thread token consumption, context window
//! usage, compaction limits, and token breakdowns (input, cached input, output,
//! reasoning), natively tailored to the active CLI engine.

use gpui::{
    AnyElement, App, IntoElement, ParentElement, PathBuilder, SharedString, Styled, Window, canvas,
    div, point, prelude::*, px,
};
use komet_proto::{ContextUsageSource, ContextUsageStats, HarnessId, format_tokens};

use crate::popover;
use crate::theme::Theme;

const RING_DIAMETER: f32 = 20.0;
const RING_STROKE: f32 = 1.75;
const ARC_SEGMENTS: u32 = 64;

fn usage_fill(ratio: f32, theme: &Theme) -> gpui::Hsla {
    if ratio >= 0.90 {
        theme.danger
    } else if ratio >= 0.70 {
        theme.warning
    } else {
        theme.text
    }
}

fn compact_ratio(stats: &ContextUsageStats) -> f32 {
    stats
        .compact_threshold
        .map(|t| (t as f32 / stats.context_limit.max(1) as f32).clamp(0.0, 1.0))
        .unwrap_or(0.75)
}

fn paint_circle(cx: f32, cy: f32, radius: f32, segments: u32) -> PathBuilder {
    let mut path = PathBuilder::fill();
    path.move_to(point(px(cx + radius), px(cy)));
    for i in 1..=segments {
        let angle = std::f32::consts::PI * 2.0 * i as f32 / segments as f32;
        path.line_to(point(
            px(cx + radius * angle.cos()),
            px(cy + radius * angle.sin()),
        ));
    }
    path.close();
    path
}

fn paint_ring_track(cx: f32, cy: f32, radius: f32, stroke: f32, segments: u32) -> PathBuilder {
    let mut path = PathBuilder::stroke(px(stroke));
    path.move_to(point(px(cx + radius), px(cy)));
    for i in 1..=segments {
        let angle = std::f32::consts::PI * 2.0 * i as f32 / segments as f32;
        path.line_to(point(
            px(cx + radius * angle.cos()),
            px(cy + radius * angle.sin()),
        ));
    }
    path.close();
    path
}

fn paint_arc(
    cx: f32,
    cy: f32,
    radius: f32,
    start: f32,
    end: f32,
    stroke: f32,
    segments: u32,
) -> PathBuilder {
    let mut path = PathBuilder::stroke(px(stroke));
    path.move_to(point(
        px(cx + radius * start.cos()),
        px(cy + radius * start.sin()),
    ));
    let sweep = end - start;
    for i in 1..=segments {
        let t = i as f32 / segments as f32;
        let angle = start + sweep * t;
        path.line_to(point(
            px(cx + radius * angle.cos()),
            px(cy + radius * angle.sin()),
        ));
    }
    path
}

/// Render the circular context usage ring indicator (trigger widget).
/// Track + clockwise arc from 12 o'clock, with a faint inner wash so fill
/// reads at 20px. Optional percent sits beside it for at-a-glance reading.
pub fn render_context_ring(stats: &ContextUsageStats, theme: &Theme) -> AnyElement {
    let _child = crate::perf::child_frame(crate::perf::ChildView::ContextUsage);
    let ratio = stats.context_ratio();
    let percent = stats.context_percent();
    let show_percent = stats.used() > 0;

    let ring = canvas(
        move |_bounds, _window, _cx| (),
        move |bounds, _, window, cx| {
            let theme = Theme::of(cx);
            let fill = usage_fill(ratio, theme);
            let center_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0;
            let center_y = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
            let radius = (f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 2.0)
                - RING_STROKE;

            if let Ok(built) = paint_circle(center_x, center_y, radius - 1.5, 36).build() {
                window.paint_path(built, fill.opacity((ratio * 0.18).clamp(0.04, 0.18)));
            }

            if let Ok(built) =
                paint_ring_track(center_x, center_y, radius, RING_STROKE, ARC_SEGMENTS).build()
            {
                window.paint_path(built, theme.border);
            }

            if ratio > 0.001 {
                let start = -std::f32::consts::PI / 2.0;
                let end = start + ratio * std::f32::consts::PI * 2.0;
                let segs = ((ARC_SEGMENTS as f32 * ratio).ceil() as u32).clamp(2, ARC_SEGMENTS);
                if let Ok(built) = paint_arc(
                    center_x,
                    center_y,
                    radius,
                    start,
                    end,
                    RING_STROKE + 0.35,
                    segs,
                )
                .build()
                {
                    window.paint_path(built, fill);
                }
            }
        },
    )
    .size(px(RING_DIAMETER));

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .child(ring)
        .when(show_percent, |el| {
            el.child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .child(format!("{percent}%")),
            )
        })
        .into_any_element()
}

/// Render the detailed Context popover: header + fluid progress bar, a
/// "Thread usage" breakdown, and a compactations summary. Unbranded — same
/// tokens as the other picker cards (hairline separators, mono figures,
/// muted labels). `harness`/`model_name` stay in the signature for call-site
/// stability.
pub fn render_context_popover(
    stats: &ContextUsageStats,
    thread: &crate::state::ThreadCompactions,
    _harness: Option<HarnessId>,
    _model_name: Option<&str>,
    theme: &Theme,
) -> AnyElement {
    let ratio = stats.context_ratio();
    let percent = stats.context_percent();
    let window_used = stats.used();
    let total_used = stats.total_tokens();
    let fill_color = usage_fill(ratio, theme);
    let threshold = compact_ratio(stats);
    let compact_at = stats
        .compact_threshold
        .map(format_tokens)
        .unwrap_or_else(|| format_tokens(stats.context_limit.saturating_mul(3) / 4));

    let has_breakdown =
        total_used > 0 || stats.cached_input_tokens > 0 || stats.reasoning_tokens > 0;
    // The counter and the latest reason are the same fact the timeline draws,
    // seen twice: only some harnesses report the native numbers, and the doc
    // is never silent once a boundary landed. Reading both keeps the popover
    // from contradicting the rows below it.
    let compaction_count = stats.compactions_count.max(thread.count as u32);
    let compaction_reason = stats
        .compactions_reason
        .clone()
        .or_else(|| thread.last_trigger.clone());
    let has_compactions = compaction_count > 0 || thread.in_progress || compaction_reason.is_some();

    let header = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .px(px(8.0))
        .pt(px(8.0))
        .pb(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Context"),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(px(13.0))
                        .text_color(fill_color)
                        .child(format!("{percent}%")),
                ),
        )
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(format!(
                    "{} / {}",
                    format_tokens(window_used),
                    format_tokens(stats.context_limit)
                )),
        )
        .child(usage_track(ratio, threshold, fill_color, theme))
        .child(plain_row("Compacts at", compact_at, theme));

    let mut root = div().flex().flex_col().child(header);

    if has_breakdown {
        let input_share = if total_used == 0 {
            0.0
        } else {
            stats.input_tokens as f32 / total_used as f32
        };
        let mut thread = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(8.0))
            .pb(px(6.0))
            .child(section_header(
                "Thread usage",
                format_tokens(total_used),
                theme,
            ));
        if total_used > 0 {
            thread = thread.child(composition_track(input_share, theme));
        }
        thread = thread
            .child(plain_row("Input", format_tokens(stats.input_tokens), theme))
            .when(stats.cached_input_tokens > 0, |el| {
                el.child(plain_row(
                    "Cached",
                    format_tokens(stats.cached_input_tokens),
                    theme,
                ))
            })
            .child(plain_row(
                "Output",
                format_tokens(stats.output_tokens),
                theme,
            ))
            .when(stats.reasoning_tokens > 0, |el| {
                el.child(plain_row(
                    "Reasoning",
                    format_tokens(stats.reasoning_tokens),
                    theme,
                ))
            });
        root = root.child(popover::menu_separator()).child(thread);
    }

    if has_compactions {
        let count = if thread.in_progress {
            format!("{compaction_count} · in progress")
        } else {
            compaction_count.to_string()
        };
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(8.0))
            .pb(px(6.0))
            .child(section_header("Compactions", count, theme));
        // The reason only ever comes from a real report — the agent's stats
        // frame or the boundary itself — never a default.
        if let Some(reason) = compaction_reason {
            section = section.child(plain_row("Latest", reason, theme));
        }
        // The gauge and the marker are two views of one event; the link is how
        // the user finds the thread's own account of it.
        if compaction_count > 0 || thread.in_progress {
            section = section.child(
                div()
                    .id("context-see-compaction")
                    .cursor_pointer()
                    .pt(px(2.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .hover(|s| s.text_color(theme.text_muted))
                    .on_click(move |_, window: &mut Window, cx: &mut App| {
                        window
                            .dispatch_action(Box::new(crate::transcript::RevealLastCompaction), cx);
                    })
                    .child("See in thread"),
            );
        }
        root = root.child(popover::menu_separator()).child(section);
    }

    root.child(
        div()
            .px(px(8.0))
            .pt(px(2.0))
            .pb(px(8.0))
            .text_size(px(11.0))
            .line_height(px(15.0))
            .text_color(theme.text_faint)
            .child(usage_source_copy(stats.source)),
    )
    .into_any_element()
}

/// Full-width context fill with a compact-threshold tick. Widths are
/// relative so the bar follows the card instead of a hardcoded px width.
fn usage_track(ratio: f32, threshold: f32, fill: gpui::Hsla, theme: &Theme) -> impl IntoElement {
    let fill_width = if ratio > 0.0 {
        ratio.clamp(0.02, 1.0)
    } else {
        0.0
    };
    div()
        .w_full()
        .h(px(8.0))
        .relative()
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(2.0))
                .h(px(4.0))
                .rounded_full()
                .bg(theme.border)
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fill_width))
                        .rounded_full()
                        .bg(fill),
                ),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_row()
                .child(div().w(gpui::relative(threshold)).h_full())
                .child(
                    div()
                        .w(px(1.5))
                        .ml(px(-0.75))
                        .h_full()
                        .rounded_full()
                        .bg(theme.warning.opacity(0.85)),
                ),
        )
}

/// Input vs output share of thread tokens — same track recipe, quieter fill.
fn composition_track(input_share: f32, theme: &Theme) -> impl IntoElement {
    let input = input_share.clamp(0.0, 1.0);
    let output = (1.0 - input).max(0.0);
    div()
        .w_full()
        .h(px(3.0))
        .rounded_full()
        .bg(theme.border)
        .overflow_hidden()
        .flex()
        .flex_row()
        .when(input > 0.0, |el| {
            el.child(
                div()
                    .h_full()
                    .w(gpui::relative(input))
                    .bg(theme.text.opacity(0.85)),
            )
        })
        .when(output > 0.0, |el| {
            el.child(
                div()
                    .h_full()
                    .w(gpui::relative(output))
                    .bg(theme.text.opacity(0.35)),
            )
        })
}

fn usage_source_copy(source: ContextUsageSource) -> &'static str {
    match source {
        ContextUsageSource::Native => "Provider-reported context window for this session.",
        ContextUsageSource::Approximate => {
            "Approximate fill from agents that do not report a window (last-turn occupancy, capped)."
        }
        ContextUsageSource::Estimated => {
            "Estimated from the transcript (about 4 characters per token)."
        }
    }
}

fn section_header(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .justify_between()
        .pt(px(2.0))
        .pb(px(4.0))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(popover::tracked_upper(label))),
        )
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

fn plain_row(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .py(px(2.0))
        .text_size(px(12.0))
        .child(div().min_w_0().text_color(theme.text_muted).child(label))
        .child(
            div()
                .flex_none()
                .font_family(theme.font_mono.clone())
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_fill_follows_theme_tokens() {
        let theme = Theme::dark();
        assert_eq!(usage_fill(0.2, &theme), theme.text);
        assert_eq!(usage_fill(0.75, &theme), theme.warning);
        assert_eq!(usage_fill(0.95, &theme), theme.danger);
    }
}
