//! Palette — komet's, not an invention.
//!
//! Every value here is the sRGB conversion of the exact oklch the gpui viewport
//! paints (`komet-ui/src/theme.rs`), so the two surfaces are the same product.
//! The tests at the bottom pin the conversions; `komet_proto::view::dot` holds
//! the status hues both frontends read, because a dot that means "running" in
//! one place cannot mean something else in the other.
//!
//! Two rules the original enforces and this follows:
//!
//! 1. **Near-monochrome.** Text is a three-step neutral ramp. Color appears only
//!    on status dots and, per the original's own note, *indigo is reserved for
//!    Send*. Nothing else is tinted — no colored tool names, no green code.
//! 2. **Never paint a background we don't need.** komet's surfaces differ by
//!    seven values out of 255 (`#060606` main, `#0d0d0d` sidebar) — invisible in
//!    a terminal, and every non-default cell is an SGR sequence ratatui must
//!    emit. Only the selected row, the user bubble, and inline code get a wash,
//!    which is also what lets the user's own terminal background show through.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Primary text — neutral(0.922).
    pub text: Color,
    /// Secondary text: sub-lines, section labels — neutral(0.708).
    pub muted: Color,
    /// Tertiary: timestamps, hints, hairlines — neutral(0.556).
    pub faint: Color,
    /// Hairline rules and pane dividers. Dimmer than `faint`, as in the original
    /// where borders are white at 8% alpha.
    pub hairline: Color,
    /// Indigo. Reserved for Send and focus, per the original.
    pub accent: Color,
    /// Selected-row wash — white@8% over the sidebar surface.
    pub selection: Color,
    /// Raised surface: the user bubble, tool chips — neutral(0.235).
    pub raised: Color,
    /// Inline-code wash — white@6% over the main surface.
    pub code_wash: Color,
    pub danger: Color,
    pub warning: Color,
    // Status dots. Same glyph everywhere; only the color differs.
    pub dot_working: Color,
    pub dot_awaiting: Color,
    pub dot_errored: Color,
    pub dot_completed: Color,
    pub dot_idle: Color,
    /// True when colors were suppressed — renderers lean on modifiers instead.
    pub plain: bool,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            text: Color::Rgb(0xe5, 0xe5, 0xe5),
            muted: Color::Rgb(0xa1, 0xa1, 0xa1),
            faint: Color::Rgb(0x73, 0x73, 0x73),
            hairline: Color::Rgb(0x3a, 0x3a, 0x3a),
            accent: Color::Rgb(0x7c, 0x86, 0xff),
            selection: Color::Rgb(0x20, 0x20, 0x20),
            raised: Color::Rgb(0x1e, 0x1e, 0x1e),
            code_wash: Color::Rgb(0x15, 0x15, 0x15),
            danger: Color::Rgb(0xff, 0x64, 0x67),
            warning: Color::Rgb(0xff, 0xb9, 0x00),
            dot_working: Color::Rgb(0xfb, 0x64, 0xb6),
            dot_awaiting: Color::Rgb(0x7c, 0x86, 0xff),
            dot_errored: Color::Rgb(0xff, 0x64, 0x67),
            dot_completed: Color::Rgb(0x00, 0xd4, 0x92),
            dot_idle: Color::Rgb(0x2f, 0x2f, 0x2f),
            plain: false,
        }
    }

    /// `NO_COLOR`: every hue collapses to the terminal default. Callers that
    /// carry meaning in color also set a [`Modifier`], so nothing is lost.
    pub fn plain() -> Self {
        let reset = Color::Reset;
        Self {
            text: reset,
            muted: reset,
            faint: reset,
            hairline: reset,
            accent: reset,
            selection: reset,
            raised: reset,
            code_wash: reset,
            danger: reset,
            warning: reset,
            dot_working: reset,
            dot_awaiting: reset,
            dot_errored: reset,
            dot_completed: reset,
            dot_idle: reset,
            plain: true,
        }
    }

    /// `NO_COLOR` set to anything non-empty disables color
    /// ([no-color.org](https://no-color.org)).
    pub fn from_env() -> Self {
        match std::env::var_os("NO_COLOR") {
            Some(v) if !v.is_empty() => Self::plain(),
            _ => Self::dark(),
        }
    }

    pub fn body(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn subtle(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn hint(&self) -> Style {
        Style::default().fg(self.faint)
    }

    pub fn rule(&self) -> Style {
        Style::default().fg(self.hairline)
    }

    /// Section labels ("Sessions") and the focused pane's edge.
    pub fn label(&self) -> Style {
        let style = Style::default().fg(self.muted);
        if self.plain {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    /// A selected sidebar row. Under `NO_COLOR` reversed video carries it.
    pub fn selected(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(self.text).bg(self.selection)
        }
    }

    /// The user's own message — komet's raised bubble.
    pub fn bubble(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(self.text).bg(self.raised)
        }
    }

    /// Inline `code`: a wash, as in the original — not a hue.
    pub fn code(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.text).bg(self.code_wash)
        }
    }

    /// The dot color for a chat status.
    pub fn dot(&self, status: komet_proto::ChatIndicator) -> Style {
        use komet_proto::ChatIndicator as I;
        let color = match status {
            I::Working => self.dot_working,
            I::AwaitingInput => self.dot_awaiting,
            I::Errored => self.dot_errored,
            I::Completed => self.dot_completed,
            I::Idle => self.dot_idle,
        };
        let style = Style::default().fg(color);
        // Under NO_COLOR the dot can't carry state, so bold the two that want
        // attention and leave the rest plain.
        if self.plain && matches!(status, I::AwaitingInput | I::Errored) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

/// The status dot. One glyph for every state — "status is a rail, not a word"
/// (komet's session-row): rows stay aligned and a state change reads in place.
pub const DOT: &str = "●";

/// How often the render loop wakes while something is animating. The loaders
/// are pure functions of elapsed time, so this only sets smoothness, not
/// correctness — a missed tick shifts nothing.
pub const ANIMATION_TICK_MS: u64 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// The oklch → sRGB conversion, duplicated from `komet-ui`'s theme so this
    /// crate can assert its literals without depending on gpui.
    fn oklch(l: f32, c: f32, h_deg: f32) -> Color {
        let h = h_deg.to_radians();
        let (a, b) = (c * h.cos(), c * h.sin());
        let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
        let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
        let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
        let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
        let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
        let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
        let bl = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;
        let enc = |x: f32| {
            let x = x.clamp(0.0, 1.0);
            let v = if x <= 0.003_130_8 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
            (v * 255.0).round() as u8
        };
        Color::Rgb(enc(r), enc(g), enc(bl))
    }

    #[test]
    fn the_palette_is_komets() {
        // If the desktop theme moves, these fail and say so — the whole point of
        // porting values rather than eyeballing them.
        let t = Theme::dark();
        assert_eq!(t.text, oklch(0.922, 0.0, 0.0), "neutral-200 body text");
        assert_eq!(t.muted, oklch(0.708, 0.0, 0.0), "neutral-400 sub-lines");
        assert_eq!(t.faint, oklch(0.556, 0.0, 0.0), "neutral-500 timestamps");
        assert_eq!(t.raised, oklch(0.235, 0.0, 0.0), "raised surface");
        assert_eq!(t.accent, oklch(0.673, 0.182, 276.935), "indigo-400");
        assert_eq!(t.danger, oklch(0.704, 0.191, 22.216), "red-400");
        assert_eq!(t.warning, oklch(0.828, 0.189, 84.429), "amber-400");
    }

    #[test]
    fn status_dots_match_the_shared_hues() {
        // The meanings live in proto so they cannot diverge per surface.
        let t = Theme::dark();
        let d = komet_proto::view::dot::WORKING;
        assert_eq!(t.dot_working, oklch(d.0, d.1, d.2), "pink, not amber");
        let d = komet_proto::view::dot::AWAITING;
        assert_eq!(t.dot_awaiting, oklch(d.0, d.1, d.2));
        let d = komet_proto::view::dot::ERRORED;
        assert_eq!(t.dot_errored, oklch(d.0, d.1, d.2));
        let d = komet_proto::view::dot::COMPLETED;
        assert_eq!(t.dot_completed, oklch(d.0, d.1, d.2), "emerald");
        // Awaiting is the accent, exactly as in the original.
        assert_eq!(t.dot_awaiting, t.accent);
    }

    #[test]
    fn no_color_keeps_meaning_in_modifiers() {
        let t = Theme::plain();
        assert!(t.plain);
        // Nothing paints a hue…
        for color in [t.text, t.accent, t.dot_working, t.selection, t.raised] {
            assert_eq!(color, Color::Reset);
        }
        // …so the states that need attention carry a modifier instead.
        use komet_proto::ChatIndicator as I;
        assert!(
            t.dot(I::AwaitingInput)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(t.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(t.bubble().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn only_washes_paint_a_background() {
        // Every non-default background cell is an escape sequence, so the set of
        // styles that use one is deliberately tiny.
        let t = Theme::dark();
        for style in [t.body(), t.subtle(), t.hint(), t.rule(), t.label()] {
            assert_eq!(style.bg, None, "body styles must not paint a background");
        }
        for style in [t.selected(), t.bubble(), t.code()] {
            assert!(style.bg.is_some());
        }
    }
}
