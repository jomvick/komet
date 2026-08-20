//! Clean static Komet logo mark & companion SVG icon.

use gpui::{AnyElement, IntoElement, Styled, px};

use crate::icons;

/// Helper function to create a static Komet logo mark / companion element.
pub fn komet(_id: &'static str, size_px: f32, color: Option<gpui::Hsla>) -> AnyElement {
    let el = icons::icon(icons::KOMET_LOGO).size(px(size_px));
    if let Some(c) = color {
        el.text_color(c).into_any_element()
    } else {
        el.into_any_element()
    }
}

/// Backward-compatible alias for the companion.
pub fn blobatar(id: &'static str, size_px: f32, color: Option<gpui::Hsla>) -> AnyElement {
    komet(id, size_px, color)
}

/// The static Komet mark component.
#[derive(Clone)]
pub struct KometElement {
    pub id: &'static str,
    pub size: f32,
    pub color: Option<gpui::Hsla>,
}

impl KometElement {
    pub fn new(id: &'static str, size: f32) -> Self {
        Self {
            id,
            size,
            color: None,
        }
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: gpui::Hsla) -> Self {
        self.color = Some(color);
        self
    }

    pub fn interactive(self, _interactive: bool) -> Self {
        self
    }
}

impl IntoElement for KometElement {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        komet(self.id, self.size, self.color)
    }
}
