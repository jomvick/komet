//! [`edge_faded`] — wraps a child so its whole subtree paints inside a
//! [`gpui::EdgeFade`] scope: primitives fade by vertical distance to the
//! wrapper's own top/bottom edges (per-glyph granularity — a true static
//! gradient, unlike whole-row opacity). Built for the GLASS sidebar's scroll
//! fade: over a see-through blurred backdrop no painted overlay can fade
//! content out, because "what is behind the window" is not a paintable color.

use gpui::{
    AnyElement, App, Bounds, EdgeFade, Element, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window, px,
};

/// Fade the child's content at its own edges: `top`/`bottom` select which
/// edges (pass the "is there hidden overflow" flags), `band` is the ramp
/// height in px. Horizontal edges via [`EdgeFaded::fade_left`] /
/// [`EdgeFaded::fade_right`].
pub fn edge_faded(band: f32, top: bool, bottom: bool, child: impl IntoElement) -> EdgeFaded {
    EdgeFaded {
        band,
        band_top: None,
        band_bottom: None,
        inset_top: 0.0,
        top,
        bottom,
        left: false,
        right: false,
        child: child.into_any_element(),
    }
}

pub struct EdgeFaded {
    band: f32,
    band_top: Option<f32>,
    band_bottom: Option<f32>,
    inset_top: f32,
    top: bool,
    bottom: bool,
    left: bool,
    right: bool,
    child: AnyElement,
}

impl EdgeFaded {
    pub fn fade_left(mut self, on: bool) -> Self {
        self.left = on;
        self
    }

    pub fn fade_right(mut self, on: bool) -> Self {
        self.right = on;
        self
    }

    /// Override the ramp height at the TOP edge only. Asymmetric bands let
    /// content fade across chrome of different heights — a short titlebar
    /// above vs a tall composer stack below.
    pub fn band_top(mut self, px: f32) -> Self {
        self.band_top = Some(px);
        self
    }

    /// [`Self::band_top`], for the bottom edge.
    pub fn band_bottom(mut self, px: f32) -> Self {
        self.band_bottom = Some(px);
        self
    }

    /// Pull the fade's TOP edge `px` inside the wrapper: gpui clamps the ramp
    /// to 0 past an active edge, so content between the wrapper's real top
    /// and the inset edge paints fully transparent — content under opaque-ish
    /// chrome (titlebar TEXT) vanishes before it can overlap.
    pub fn inset_top(mut self, px: f32) -> Self {
        self.inset_top = px;
        self
    }
}

impl Element for EdgeFaded {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let fade = (self.top || self.bottom || self.left || self.right).then(|| {
            let mut bounds = bounds;
            bounds.origin.y += px(self.inset_top);
            bounds.size.height -= px(self.inset_top);
            EdgeFade {
                bounds,
                band: px(self.band),
                band_top: self.band_top.map(px),
                band_bottom: self.band_bottom.map(px),
                top: self.top,
                bottom: self.bottom,
                left: self.left,
                right: self.right,
            }
        });
        window.with_edge_fade(fade, |window| self.child.paint(window, cx));
    }
}

impl IntoElement for EdgeFaded {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
