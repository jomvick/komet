//! Context & Token Usage telemetry visualization: circular progress sphere + dedicated CLI popovers.
//!
//! Provides real-time visibility into thread token consumption, context window usage,
//! compaction limits, and token breakdowns (input, cached input, output, reasoning),
//! natively tailored to the active CLI engine (Claude Code, OpenCode, Codex, etc.).

use gpui::{
    AnyElement, IntoElement, ParentElement, PathBuilder, SharedString, Styled, canvas, div, hsla,
    point, px,
};
use komet_proto::{ContextUsageStats, HarnessId, format_tokens};

use crate::theme::Theme;

const RING_DIAMETER: f32 = 20.0;
const RING_STROKE: f32 = 1.6;
const RING_INNER_RADIUS: f32 = (RING_DIAMETER / 2.0) - RING_STROKE;

/// Telemetry profile and branding identity for a specific CLI engine.
///
/// Only [`render_context_ring`] and [`render_hero_sphere`] read the brand
/// color today — the popover itself (see [`render_context_popover`]) is
/// deliberately unbranded, a plain "Context" card, per design reference
/// 2026-09-01 (a prior branded/card redesign was reverted).
pub struct HarnessContextProfile {
    pub display_name: &'static str,
    pub protocol_badge: &'static str,
    pub brand_icon: &'static str,
    pub brand_color: gpui::Hsla,
    pub telemetry_title: &'static str,
    pub telemetry_description: &'static str,
    pub caching_type: Option<&'static str>,
    pub default_compaction_rule: &'static str,
}

impl HarnessContextProfile {
    pub fn for_harness(harness: Option<HarnessId>, _theme: &Theme) -> Self {
        match harness {
            Some(HarnessId::ClaudeCode) => Self {
                display_name: "Claude Code",
                protocol_badge: "Native CLI",
                brand_icon: crate::icons::CLAUDE_MARK,
                brand_color: crate::icons::claude_brand(),
                telemetry_title: "Anthropic Prompt Caching & Auto-Compact",
                telemetry_description:
                    "Uses 5-min ephemeral prompt caching. Auto-compaction triggers at ~75-80% context window.",
                caching_type: Some("Anthropic Prompt Cache (5m TTL)"),
                default_compaction_rule: "Auto-compact at 150k / 75%",
            },
            Some(HarnessId::Opencode) => Self {
                display_name: "OpenCode",
                protocol_badge: "Native ACP",
                brand_icon: crate::icons::OPENCODE_MARK,
                brand_color: hsla(158.0 / 360.0, 0.85, 0.44, 1.0),
                telemetry_title: "OpenCode ACP Runtime Telemetry",
                telemetry_description:
                    "Native ACP protocol telemetry across subagents, tool executions, and multi-model context.",
                caching_type: Some("ACP Turn Session Buffer"),
                default_compaction_rule: "Turn boundary prune",
            },
            Some(HarnessId::Codex) => Self {
                display_name: "Codex",
                protocol_badge: "App Protocol",
                brand_icon: crate::icons::OPENAI_MARK,
                brand_color: hsla(160.0 / 360.0, 0.82, 0.35, 1.0),
                telemetry_title: "OpenAI App Protocol & CoT Telemetry",
                telemetry_description:
                    "Direct thread token telemetry with granular tracking for reasoning output (o1/o3/gpt-4o) and turn delta.",
                caching_type: Some("OpenAI Prefix Caching"),
                default_compaction_rule: "Sliding turn context window",
            },
            Some(HarnessId::Cursor) => Self {
                display_name: "Cursor",
                protocol_badge: "Agent CLI",
                brand_icon: crate::icons::CURSOR_MARK,
                brand_color: hsla(217.0 / 360.0, 0.91, 0.60, 1.0),
                telemetry_title: "Cursor Agent Telemetry",
                telemetry_description:
                    "Cursor workspace index and agent tool execution telemetry.",
                caching_type: Some("Workspace Index Cache"),
                default_compaction_rule: "Context window boundary",
            },
            Some(HarnessId::Grok) => Self {
                display_name: "Grok",
                protocol_badge: "xAI ACP",
                brand_icon: crate::icons::GROK_MARK,
                brand_color: hsla(350.0 / 360.0, 0.80, 0.55, 1.0),
                telemetry_title: "xAI Grok Agent Runtime",
                telemetry_description:
                    "Grok ACP stdio agent context window tracking and tool execution telemetry.",
                caching_type: None,
                default_compaction_rule: "Model context limit",
            },
            Some(HarnessId::Hermes) => Self {
                display_name: "Hermes",
                protocol_badge: "Nous ACP",
                brand_icon: crate::icons::HERMES_MARK,
                brand_color: hsla(270.0 / 360.0, 0.80, 0.65, 1.0),
                telemetry_title: "Nous Research Hermes Runtime",
                telemetry_description:
                    "Hermes ACP turn execution and tool calling context window metrics.",
                caching_type: None,
                default_compaction_rule: "Turn boundary prune",
            },
            Some(HarnessId::Pi) => Self {
                display_name: "Pi",
                protocol_badge: "Pi ACP",
                brand_icon: crate::icons::PI_MARK,
                brand_color: hsla(38.0 / 360.0, 0.92, 0.50, 1.0),
                telemetry_title: "Pi.dev Coding Agent",
                telemetry_description:
                    "Pi ACP adapter telemetry with session memory management.",
                caching_type: None,
                default_compaction_rule: "Session limit",
            },
            Some(HarnessId::Antigravity) => Self {
                display_name: "Antigravity",
                protocol_badge: "Native Engine",
                brand_icon: crate::icons::ANTIGRAVITY_MARK,
                brand_color: hsla(217.0 / 360.0, 0.90, 0.60, 1.0),
                telemetry_title: "Antigravity Controller Telemetry",
                telemetry_description:
                    "Native multi-agent controller telemetry and subagent thread metrics.",
                caching_type: Some("Multi-turn state cache"),
                default_compaction_rule: "Automatic context compaction",
            },
            _ => Self {
                display_name: "Agent Session",
                protocol_badge: "Session Telemetry",
                brand_icon: crate::icons::KOMET_LOGO,
                brand_color: hsla(215.0 / 360.0, 0.70, 0.55, 1.0),
                telemetry_title: "Thread Context & Token Telemetry",
                telemetry_description:
                    "Cumulative provider-reported usage and context-window metrics.",
                caching_type: None,
                default_compaction_rule: "Auto threshold",
            },
        }
    }
}

/// Render the circular context usage ring indicator (trigger widget).
/// Simple circle track with a white arc sweeping clockwise to show fill level.
pub fn render_context_ring(
    stats: &ContextUsageStats,
    harness: Option<HarnessId>,
    theme: &Theme,
) -> AnyElement {
    let ratio = stats.context_ratio();
    // Keep profile for popover but use only white fill for the ring
    let _ = HarnessContextProfile::for_harness(harness, theme);

    // Arc fill color: white at different opacities based on severity
    let arc_color = if ratio >= 0.90 {
        hsla(0.0 / 360.0, 0.80, 0.60, 1.0)  // red when critical
    } else if ratio >= 0.70 {
        hsla(38.0 / 360.0, 0.90, 0.65, 1.0) // amber when high
    } else {
        hsla(0.0, 0.0, 1.0, 0.90)            // clean white normally
    };

    canvas(
        move |_bounds, _window, _cx| (),
        move |bounds, _, window, cx| {
            let theme = Theme::of(cx);
            let center_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0;
            let center_y = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
            let radius = RING_INNER_RADIUS;

            // 1. Track circle (dim background ring)
            let mut track_path = PathBuilder::stroke(px(RING_STROKE));
            track_path.move_to(point(px(center_x + radius), px(center_y)));
            for i in 1..=32 {
                let angle = std::f32::consts::PI * 2.0 * i as f32 / 32.0;
                let x = center_x + radius * angle.cos();
                let y = center_y + radius * angle.sin();
                track_path.line_to(point(px(x), px(y)));
            }
            track_path.close();
            if let Ok(built) = track_path.build() {
                window.paint_path(built, theme.border);
            }

            // 2. White fill arc sweeping clockwise from 12 o'clock
            if ratio > 0.001 {
                let mut progress_path = PathBuilder::stroke(px(RING_STROKE * 1.5));
                let start_angle = -std::f32::consts::PI / 2.0; // 12 o'clock
                let end_angle = start_angle + ratio * std::f32::consts::PI * 2.0;
                let segments = 32;
                progress_path.move_to(point(
                    px(center_x + radius * start_angle.cos()),
                    px(center_y + radius * start_angle.sin()),
                ));
                for i in 1..=segments {
                    let t = i as f32 / segments as f32;
                    let angle = start_angle + (end_angle - start_angle) * t;
                    let x = center_x + radius * angle.cos();
                    let y = center_y + radius * angle.sin();
                    progress_path.line_to(point(px(x), px(y)));
                }
                if let Ok(built) = progress_path.build() {
                    window.paint_path(built, arc_color);
                }
            }
        },
    )
    .size(px(RING_DIAMETER))
    .into_any_element()
}

/// Render the large Hero 3D Holographic Context Sphere for the detailed popover.
pub fn render_hero_sphere(
    stats: &ContextUsageStats,
    profile: &HarnessContextProfile,
    theme: &Theme,
) -> AnyElement {
    let ratio = stats.context_ratio();
    let brand_color = profile.brand_color;

    let fill_color = if ratio >= 0.90 {
        hsla(0.0 / 360.0, 0.85, 0.55, 1.0)
    } else if ratio >= 0.70 {
        hsla(38.0 / 360.0, 0.92, 0.50, 1.0)
    } else if ratio > 0.0 {
        brand_color
    } else {
        theme.text_muted
    };

    let core_base_color = if ratio >= 0.90 {
        hsla(0.0 / 360.0, 0.85, 0.50, 0.95)
    } else if ratio >= 0.70 {
        hsla(38.0 / 360.0, 0.92, 0.45, 0.90)
    } else if ratio > 0.0 {
        brand_color.opacity(0.85)
    } else {
        theme.text_muted.opacity(0.35)
    };

    let highlight_color = hsla(0.0, 0.0, 1.0, if ratio > 0.0 { 0.85 } else { 0.50 });
    let threshold_ratio = stats
        .compact_threshold
        .map(|t| (t as f32 / stats.context_limit.max(1) as f32).clamp(0.0, 1.0))
        .unwrap_or(0.75);

    canvas(
        move |_bounds, _window, _cx| (),
        move |bounds, _, window, cx| {
            let theme = Theme::of(cx);
            let center_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0;
            let center_y = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
            let orbit_radius = 21.0;

            // 1. Atmospheric Ambient Glow
            let halo_radius = orbit_radius + 3.0;
            let mut halo_path = PathBuilder::fill();
            halo_path.move_to(point(px(center_x + halo_radius), px(center_y)));
            for i in 1..=32 {
                let angle = std::f32::consts::PI * 2.0 * i as f32 / 32.0;
                let x = center_x + halo_radius * angle.cos();
                let y = center_y + halo_radius * angle.sin();
                halo_path.line_to(point(px(x), px(y)));
            }
            halo_path.close();
            if let Ok(built) = halo_path.build() {
                window.paint_path(built, brand_color.opacity(0.10));
            }

            // 2. Orbital Background Track
            let mut orbit_track = PathBuilder::stroke(px(2.0));
            orbit_track.move_to(point(px(center_x + orbit_radius), px(center_y)));
            for i in 1..=48 {
                let angle = std::f32::consts::PI * 2.0 * i as f32 / 48.0;
                let x = center_x + orbit_radius * angle.cos();
                let y = center_y + orbit_radius * angle.sin();
                orbit_track.line_to(point(px(x), px(y)));
            }
            orbit_track.close();
            if let Ok(built) = orbit_track.build() {
                window.paint_path(built, theme.border);
            }

            // 3. Compaction threshold tick notch on orbit
            let threshold_angle = -std::f32::consts::PI / 2.0 + threshold_ratio * std::f32::consts::PI * 2.0;
            let tick_inner_r = orbit_radius - 3.5;
            let tick_outer_r = orbit_radius + 3.5;
            let mut tick_path = PathBuilder::stroke(px(1.5));
            tick_path.move_to(point(
                px(center_x + tick_inner_r * threshold_angle.cos()),
                px(center_y + tick_inner_r * threshold_angle.sin()),
            ));
            tick_path.line_to(point(
                px(center_x + tick_outer_r * threshold_angle.cos()),
                px(center_y + tick_outer_r * threshold_angle.sin()),
            ));
            if let Ok(built) = tick_path.build() {
                window.paint_path(built, hsla(38.0 / 360.0, 0.92, 0.50, 0.85));
            }

            // 4. Active Orbital Capacity Arc
            if ratio > 0.001 {
                let mut progress_path = PathBuilder::stroke(px(2.8));
                let start_angle = -std::f32::consts::PI / 2.0; // 12 o'clock
                let end_angle = start_angle + ratio * std::f32::consts::PI * 2.0;
                let segments = 48;
                progress_path.move_to(point(
                    px(center_x + orbit_radius * start_angle.cos()),
                    px(center_y + orbit_radius * start_angle.sin()),
                ));
                for i in 1..=segments {
                    let t = i as f32 / segments as f32;
                    let angle = start_angle + (end_angle - start_angle) * t;
                    let x = center_x + orbit_radius * angle.cos();
                    let y = center_y + orbit_radius * angle.sin();
                    progress_path.line_to(point(px(x), px(y)));
                }
                if let Ok(built) = progress_path.build() {
                    window.paint_path(built, fill_color);
                }
            }

            // 5. 3D Glass Hero Sphere Core
            let sphere_r = 13.0;
            let mut sphere_path = PathBuilder::fill();
            sphere_path.move_to(point(px(center_x + sphere_r), px(center_y)));
            for i in 1..=36 {
                let angle = std::f32::consts::PI * 2.0 * i as f32 / 36.0;
                let x = center_x + sphere_r * angle.cos();
                let y = center_y + sphere_r * angle.sin();
                sphere_path.line_to(point(px(x), px(y)));
            }
            sphere_path.close();
            if let Ok(built) = sphere_path.build() {
                window.paint_path(built, core_base_color);
            }

            // 6. Large Specular Highlight (Reflection gleam top-left)
            let hl_r = 4.2;
            let hl_cx = center_x - 3.8;
            let hl_cy = center_y - 3.8;
            let mut hl_path = PathBuilder::fill();
            hl_path.move_to(point(px(hl_cx + hl_r), px(hl_cy)));
            for i in 1..=24 {
                let angle = std::f32::consts::PI * 2.0 * i as f32 / 24.0;
                let x = hl_cx + hl_r * angle.cos();
                let y = hl_cy + hl_r * angle.sin();
                hl_path.line_to(point(px(x), px(y)));
            }
            hl_path.close();
            if let Ok(built) = hl_path.build() {
                window.paint_path(built, highlight_color);
            }
        },
    )
    .size(px(52.0))
    .into_any_element()
}

/// Render the detailed Context popover: a plain "Context" card — header +
/// progress bar, a "Thread usage" breakdown, and a "Compactions" summary.
/// Deliberately unbranded (no harness icon/name/telemetry box) — design
/// reference 2026-09-01 reverted the earlier per-harness "hero card" look
/// back to this flatter list style. `harness`/`model_name` are kept in the
/// signature for call-site stability even though this rendering ignores
/// them; [`render_context_ring`]/[`render_hero_sphere`] are where harness
/// branding still shows up (the small trigger widget).
pub fn render_context_popover(
    stats: &ContextUsageStats,
    _harness: Option<HarnessId>,
    _model_name: Option<&str>,
    theme: &Theme,
) -> AnyElement {
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

    let compact_threshold_str = stats
        .compact_threshold
        .map(format_tokens)
        .unwrap_or_else(|| format_tokens(stats.context_limit.saturating_mul(3) / 4));

    let compactions_reason_str = stats
        .compactions_reason
        .clone()
        .unwrap_or_else(|| "cache expiry".to_string());

    div()
        .w(px(300.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .p(px(14.0))
        .text_sm()
        .child(
            // ── "Context" header, progress bar, compaction threshold ────
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
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
                                .text_color(theme.text_muted)
                                .child(format!(
                                    "{} / {} · {}%",
                                    format_tokens(total_used),
                                    format_tokens(stats.context_limit),
                                    percent
                                )),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(4.0))
                        .rounded_full()
                        .bg(theme.border)
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(px((272.0 * ratio).max(if ratio > 0.0 { 3.0 } else { 0.0 })))
                                .rounded_full()
                                .bg(fill_color),
                        ),
                )
                .child(plain_row("Compacts at", compact_threshold_str, theme)),
        )
        .child(
            // ── Thread usage breakdown ──────────────────────────────────
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(section_header("Thread usage", format_tokens(total_used), theme))
                .child(plain_row("Input", format_tokens(stats.input_tokens), theme))
                .child(plain_row(
                    "Cached input (included)",
                    format_tokens(stats.cached_input_tokens),
                    theme,
                ))
                .child(plain_row("Output", format_tokens(stats.output_tokens), theme))
                .child(plain_row(
                    "Reasoning output (included)",
                    format_tokens(stats.reasoning_tokens),
                    theme,
                ))
                .child(
                    div()
                        .pt(px(2.0))
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child(
                            "Cumulative provider-reported usage in the current thread transcript.",
                        ),
                ),
        )
        .child(
            // ── Compactions summary ─────────────────────────────────────
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(section_header(
                    "Compactions",
                    stats.compactions_count.to_string(),
                    theme,
                ))
                .child(plain_row("Latest", compactions_reason_str, theme)),
        )
        .into_any_element()
}

/// A section title on the left, its running total on the right — same row
/// shape as [`plain_row`] but bold, used once per section ("Context",
/// "Thread usage", "Compactions").
fn section_header(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .justify_between()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(label),
        )
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

/// One flat metric line: muted label left, mono value right.
fn plain_row(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .text_xs()
        .child(div().text_color(theme.text_muted).child(label))
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_resolution_for_all_harnesses() {
        let theme = Theme::dark();

        let claude = HarnessContextProfile::for_harness(Some(HarnessId::ClaudeCode), &theme);
        assert_eq!(claude.display_name, "Claude Code");
        assert_eq!(claude.protocol_badge, "Native CLI");

        let opencode = HarnessContextProfile::for_harness(Some(HarnessId::Opencode), &theme);
        assert_eq!(opencode.display_name, "OpenCode");
        assert_eq!(opencode.protocol_badge, "Native ACP");

        let codex = HarnessContextProfile::for_harness(Some(HarnessId::Codex), &theme);
        assert_eq!(codex.display_name, "Codex");
        assert_eq!(codex.protocol_badge, "App Protocol");

        let cursor = HarnessContextProfile::for_harness(Some(HarnessId::Cursor), &theme);
        assert_eq!(cursor.display_name, "Cursor");

        let grok = HarnessContextProfile::for_harness(Some(HarnessId::Grok), &theme);
        assert_eq!(grok.display_name, "Grok");

        let hermes = HarnessContextProfile::for_harness(Some(HarnessId::Hermes), &theme);
        assert_eq!(hermes.display_name, "Hermes");

        let pi = HarnessContextProfile::for_harness(Some(HarnessId::Pi), &theme);
        assert_eq!(pi.display_name, "Pi");

        let antigravity = HarnessContextProfile::for_harness(Some(HarnessId::Antigravity), &theme);
        assert_eq!(antigravity.display_name, "Antigravity");

        let default_profile = HarnessContextProfile::for_harness(None, &theme);
        assert_eq!(default_profile.display_name, "Agent Session");
    }
}
