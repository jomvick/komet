//! Loaders: the komet pulse loader and the gradient matrix spinner. All motion
//! routes through `crate::motion` pure helpers, so the math is unit-tested and
//! these elements are testable-by-compile.
//!
//! Rendering pattern: each cell is its own `with_animation` repeating element
//! sharing one period; per-cell offsets come from [`motion::staggered_phase`],
//! so all cells stay phase-locked (they start on the same frame) without a
//! shared clock. Cells animate inside fixed-size slots — opacity and inner size
//! are paint-local and never move surrounding layout. Reduced motion snaps every
//! cell to its rest state automatically (gpui `reduce_motion`).

use gpui::{
    AnyElement, App, EntityId, IntoElement, ParentElement, PathBuilder, SharedString, Styled,
    Window, canvas, div, point, px,
};

use crate::motion::{self, GRADIENT_SPIN, KOMET_PULSE, PULSE_STAGGER};
use crate::theme::Theme;

// Shared with the terminal viewport (`komet_proto::motion`) so both animate the
// same loaders from the same numbers.
pub use komet_proto::motion::{
    KOMET_CELLS, MARK_CELLS, MARK_SPREAD, MATRIX_SIDE, mark_cell_stagger,
};

/// The animated komet mark (komet-loader.tsx `KometLoader`): the full logo
/// pixel grid with a light wave sweeping tail→head. Each cell rests dim
/// (opacity 0.08, scale 0.9) and flares to full as the crest passes; per-cell
/// stagger follows the flight axis. `height_px` sets the mark's height (width
/// follows the 820:940 canvas).
pub fn komet_mark_loader(
    _id: &'static str,
    theme: &Theme,
    height_px: f32,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let scale = height_px / 940.0;
    let cell = 100.0 * scale;
    let delta = motion::pulse_delta(&KOMET_PULSE, view, window, cx);
    div()
        .relative()
        .w(px(820.0 * scale))
        .h(px(height_px))
        .children(MARK_CELLS.iter().map(move |&(x, y)| {
            let stagger = mark_cell_stagger(x, y);
            // Fixed slot; the animated cell breathes inside it (paint-local).
            div()
                .absolute()
                .left(px(x * scale))
                .top(px(y * scale))
                .size(px(cell))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    // Negative CSS delay ⇒ the cell starts mid-cycle:
                    // the stagger ADDS phase (komet-loader.tsx delayFor).
                    let phase = (delta + stagger).rem_euclid(1.0);
                    div()
                        .rounded(px(16.0 * scale))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(cell * motion::pulse_scale(phase)))
                })
        }))
}

/// The komet wave loader: a row of cells pulsing opacity 0.08→1 / scale 0.9→1
/// over 2.4s with a 0.15s stagger per cell.
///
/// `id` scopes the per-cell animation state — give each loader instance a
/// distinct id.
pub fn komet_loader(
    _id: &'static str,
    theme: &Theme,
    cell_px: f32,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let slot = cell_px;
    let delta = motion::pulse_delta(&KOMET_PULSE, view, window, cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(slot / 2.0))
        .children((0..KOMET_CELLS).map(move |i| {
            // Fixed slot; the animated cell breathes inside it.
            div()
                .size(px(slot))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    let phase = motion::staggered_phase(delta, i, PULSE_STAGGER);
                    div()
                        .rounded(px(slot / 4.0))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(slot * motion::pulse_scale(phase)))
                })
        }))
}

pub use komet_proto::motion::{GSPIN_DIM, GSPIN_ROW_TINTS};

/// The gradient matrix spinner (WorkingIndicator), ported from komet's
/// gradient-spin.tsx: a 3×3 grid of round cells tinted per row from the
/// sunrise gradient. Each cell pulses opacity once per 750ms period; the
/// per-cell phase follows the "arrow-up" pattern (the pulse enters at the
/// bottom edge and converges toward the top-center cell), so the wave reads
/// as travelling upward.
pub use crate::thinking_orbs::{Dot, Line, OrbFrame, OrbState, ThinkingOrb, thinking_orb};

/// Inline mini thinking orb for status-dot / session row slots.
pub fn mini_thinking_orb(state: OrbState, size_px: f32) -> ThinkingOrb {
    ThinkingOrb::new(state, size_px)
}

/// The gradient matrix spinner / ThinkingOrb loader (WorkingIndicator).
pub fn gradient_spinner(
    _id: &'static str,
    _theme: &Theme,
    cell_px: f32,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let size = (cell_px * 6.0).max(18.0);
    ThinkingOrb::new(OrbState::Working, size).driven(view, window, cx)
}

/// A miniature ThinkingOrb sized for a status-dot slot (sessions-sidebar working rows).
pub fn mini_gradient_spinner(
    _key: impl Into<SharedString>,
    cell_px: f32,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let size = (cell_px * 5.0).max(14.0);
    ThinkingOrb::new(OrbState::Working, size).driven(view, window, cx)
}

/// Stroke width of [`upload_progress_ring`] and [`spinning_ring`].
const RING_STROKE: f32 = 2.5;
/// Polyline segments for a full circle — plenty for a ≤40px ring.
const RING_SEGMENTS: f32 = 64.0;

/// Paint one ring arc: `from..to` in fractions of a turn, clockwise from 12
/// o'clock. Shared by the progress and indeterminate rings.
fn paint_ring_arc(
    window: &mut gpui::Window,
    center: gpui::Point<gpui::Pixels>,
    radius: f32,
    from: f32,
    to: f32,
    color: gpui::Hsla,
) {
    if to - from <= 0.0 {
        return;
    }
    let steps = ((RING_SEGMENTS * (to - from)).ceil() as usize).max(2);
    let at = |i: usize| {
        let theta = -std::f32::consts::FRAC_PI_2
            + std::f32::consts::TAU * (from + (to - from) * (i as f32 / steps as f32));
        point(
            center.x + px(radius * theta.cos()),
            center.y + px(radius * theta.sin()),
        )
    };
    let mut builder = PathBuilder::stroke(px(RING_STROKE));
    builder.move_to(at(0));
    for i in 1..=steps {
        builder.line_to(at(i));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Indeterminate ring: a dim track plus a bright arc sweeping clockwise on the
/// shared pulse clock, so every mounted ring stays phase-locked. Reduced motion
/// parks the arc at 12 o'clock — a still ring still reads as "working".
pub fn spinning_ring(
    diameter: f32,
    color: gpui::Hsla,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    const SWEEP: f32 = 0.32;
    let phase = motion::pulse_delta(&KOMET_PULSE, view, window, cx);
    let track = color.opacity(0.18);
    div()
        .relative()
        .size(px(diameter))
        .flex_none()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let center = bounds.center();
                    let radius = diameter / 2.0 - RING_STROKE;
                    paint_ring_arc(window, center, radius, 0.0, 1.0, track);
                    paint_ring_arc(window, center, radius, phase, phase + SWEEP, color);
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
}

/// Radial upload-progress ring with the percent centered — overlaid on a
/// sending echo's attachment thumbnail while its bytes cross the relay.
/// A faint full track plus a bright arc growing clockwise from 12 o'clock.
pub fn upload_progress_ring(percent: u8, diameter: f32) -> AnyElement {
    let frac = f32::from(percent.min(100)) / 100.0;
    let ring = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let radius = diameter / 2.0 - RING_STROKE;
            paint_ring_arc(
                window,
                center,
                radius,
                0.0,
                1.0,
                gpui::hsla(0.0, 0.0, 1.0, 0.22),
            );
            paint_ring_arc(
                window,
                center,
                radius,
                0.0,
                frac,
                gpui::hsla(0.0, 0.0, 1.0, 0.95),
            );
        },
    )
    .absolute()
    .inset_0();
    div()
        .relative()
        .size(px(diameter))
        .flex()
        .items_center()
        .justify_center()
        .child(ring)
        .child(
            div()
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::hsla(0.0, 0.0, 1.0, 0.95))
                .child(SharedString::from(format!("{percent}%"))),
        )
        .into_any_element()
}

// Compile-time proof the specs referenced here stay wired to the catalog.
const _: () = {
    assert!(KOMET_PULSE.duration_ms == 2400);
    assert!(GRADIENT_SPIN.duration_ms == 750);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_stagger_follows_flight_axis() {
        // Tail tip (720, 0) leads: near-maximal stagger (starts deepest into
        // the cycle); head (0, 840) trails with stagger 0.
        let tail = mark_cell_stagger(720.0, 0.0);
        let head = mark_cell_stagger(0.0, 840.0);
        assert!(tail > head, "tail {tail} should lead head {head}");
        assert!((head - 0.0).abs() < 1e-6, "head stagger ≈ 0, got {head}");
        assert!(tail <= MARK_SPREAD + 1e-6, "stagger capped at SPREAD");
        // Every logo cell stays inside [0, SPREAD].
        for &(x, y) in &MARK_CELLS {
            let s = mark_cell_stagger(x, y);
            assert!(
                (0.0..=MARK_SPREAD + 1e-6).contains(&s),
                "cell ({x},{y}) stagger {s}"
            );
        }
    }
}
