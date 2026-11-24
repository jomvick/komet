use gpui::{AnyElement, Context, IntoElement, SharedString, div, px};
use komet_proto::view::GatePhase;

use super::*;
use crate::state::ConnectionStatus;
use crate::theme::Theme;

impl Shell {
    pub(super) fn render_gate_card(
        &mut self,
        phase: &GatePhase,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (title, detail, retryable) = match phase {
            GatePhase::Failed(error) => (
                "Engine unavailable",
                error.clone(),
                match self.state.read(cx).connection {
                    ConnectionStatus::Failed { retryable, .. } => retryable,
                    _ => true,
                },
            ),
            _ => (
                "Sync is not configured",
                "Configure KOMET_EDGE_URL and KOMET_SYNC_TOKEN for your self-hosted server. See docs/sync.md.".to_string(),
                false,
            ),
        };
        let mut action = div()
            .id("retry-engine")
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .text_size(px(13.0))
            .text_color(if retryable {
                theme.text
            } else {
                theme.text_muted
            })
            .child(SharedString::from(if retryable {
                "Retry"
            } else {
                "Local mode"
            }));
        if retryable {
            action = action
                .cursor_pointer()
                .hover(|s| s.bg(theme.glass_hover()))
                .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)));
        }
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(380.0))
                            .px(px(32.0))
                            .py(px(36.0))
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_card)
                            .shadow_lg()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(Theme::SPACE_MD))
                            .child(
                                div()
                                    .text_size(px(18.0))
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text_muted)
                                    .child(detail),
                            )
                            .child(action),
                    ),
            )
            .into_any_element()
    }
}

/// The connection gate's faint grid backdrop (komet styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
///
/// `pub(crate)`: the first-run onboarding reuses it so its pages keep the
/// page's visual identity (same bg, same faint grid) instead of a bare fill.
pub(crate) fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}
