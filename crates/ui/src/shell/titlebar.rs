use super::*;
use crate::icons;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (komet window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Width of the persistent top-left button cluster itself (sidebar toggle +
/// back/forward: three 24px buttons, 2px gaps).
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + 2.0 * 2.0;

/// Where the cluster's first button starts, from the window's left edge.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen) + CLUSTER_BUTTONS_WIDTH + 8.0 - container_pad)
        .max(0.0)
}

impl Shell {
    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    pub(super) fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of komet __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (komet remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self, linux_left_controls: f32) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        cluster + linux_left_controls + CLUSTER_BUTTONS_WIDTH + 10.0
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    pub(super) fn render_title_bar(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let (linux_left_controls, linux_right_controls) =
            self.linux_titlebar_controls_width(window, cx);
        match self.route {
            Route::Chat => self.render_session_title_bar(window, cx),
            Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start(linux_left_controls)))
                    .pr(px(titlebar_right_padding(
                        cfg!(target_os = "windows"),
                        linux_right_controls,
                        Theme::SPACE_LG,
                    )));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (komet's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    pub(super) fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// komet window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    pub(super) fn render_titlebar_cluster(
        &mut self,
        linux_left_controls: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        // The new-session + joins the cluster while the sidebar is collapsed
        // (fading on the sidebar width tween) — INSIDE the cluster row so it
        // shares the buttons' exact size and 2px rhythm; a separate mount in
        // the title row sat 10px off the cluster and read misaligned (user
        // report).
        let plus_alpha = self.titlebar_plus_alpha();
        let show_plus = matches!(self.route, Route::Chat) && plus_alpha > 0.01;
        div()
            .absolute()
            .top_0()
            .left(px(linux_left_controls))
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(2.0))
            .px(px(10.0))
            .children(self.titlebar_spacer(12.0))
            .child(window_control_button(
                "toggle-sidebar",
                if self.settings.sidebar_collapsed {
                    icons::SIDEBAR_MINIMALISTIC
                } else {
                    icons::SIDEBAR_MINIMALISTIC_LEFT
                },
                &theme,
                cx.entity_id(),
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                can_back,
                &theme,
                cx.entity_id(),
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                can_forward,
                &theme,
                cx.entity_id(),
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .children(show_plus.then(|| {
                div()
                    .flex_none()
                    .opacity(plus_alpha)
                    .child(window_control_button(
                        "titlebar-new-session",
                        icons::PLUS,
                        &theme,
                        cx.entity_id(),
                        cx.listener(|this, _, _, cx| this.open_new_session(cx)),
                    ))
            }))
            .into_any_element()
    }

    /// How present the titlebar's new-session + is: 0 with the sidebar open
    /// (the + lives in the sidebar header), 1 fully collapsed, riding the
    /// sidebar width tween in between.
    pub(super) fn titlebar_plus_alpha(&self) -> f32 {
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let open_width = self.settings.sidebar_width.max(1.0);
        (1.0 - sidebar_now / open_width).clamp(0.0, 1.0)
    }

    /// Native Windows caption controls integrated into Komet's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    pub(super) fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    pub(super) fn linux_titlebar_controls_width(&self, window: &Window, cx: &App) -> (f32, f32) {
        if !cfg!(target_os = "linux") {
            return (0.0, 0.0);
        }
        let controls = window.window_controls();
        let layout = linux_button_layout(cx, controls);
        let (left_has_min, left_has_max) = effective_controls(layout.left, controls);
        let (right_has_min, right_has_max) = effective_controls(layout.right, controls);
        (
            linux_control_side_width(layout.left, left_has_min, left_has_max),
            linux_control_side_width(layout.right, right_has_min, right_has_max),
        )
    }

    pub(super) fn render_linux_caption_controls(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if !cfg!(target_os = "linux") {
            return Vec::new();
        }

        let controls = window.window_controls();
        let layout = linux_button_layout(cx, controls);
        let theme = Theme::of(cx).clone();
        let maximized = window.is_maximized();
        let side = |buttons: [Option<WindowButton>; 3],
                    has_min: bool,
                    has_max: bool,
                    this: &mut Self,
                    cx: &mut Context<Self>| {
            buttons
                .into_iter()
                .flatten()
                .filter(|button| match button {
                    WindowButton::Minimize => has_min,
                    WindowButton::Maximize => has_max,
                    WindowButton::Close => true,
                })
                .map(|button| this.linux_caption_button(button, maximized, &theme, cx))
                .collect::<Vec<_>>()
        };
        let (left_has_min, left_has_max) = effective_controls(layout.left, controls);
        let (right_has_min, right_has_max) = effective_controls(layout.right, controls);
        let left = side(layout.left, left_has_min, left_has_max, self, cx);
        let right = side(layout.right, right_has_min, right_has_max, self, cx);
        let group = |buttons: Vec<AnyElement>, is_left: bool| {
            if buttons.is_empty() {
                return None;
            }
            let mut group = div()
                .absolute()
                .top_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .pt(px(Theme::TITLEBAR_TOP_PAD))
                .gap(px(2.0))
                .px(px(4.0))
                .children(buttons);
            group = if is_left {
                group.left_0()
            } else {
                group.right_0()
            };
            Some(group.into_any_element())
        };
        group(left, true)
            .into_iter()
            .chain(group(right, false))
            .collect()
    }

    pub(super) fn linux_caption_button(
        &mut self,
        button: WindowButton,
        maximized: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = match button {
            WindowButton::Minimize => "linux-window-minimize",
            WindowButton::Maximize => "linux-window-maximize",
            WindowButton::Close => "linux-window-close",
        };
        let icon_path = match button {
            WindowButton::Minimize => icons::WINDOW_MINIMIZE,
            WindowButton::Maximize if maximized => icons::WINDOW_RESTORE,
            WindowButton::Maximize => icons::WINDOW_MAXIMIZE,
            WindowButton::Close => icons::WINDOW_CLOSE,
        };
        let close = button == WindowButton::Close;
        let hover_bg: gpui::Hsla = if close {
            gpui::rgb(0xe81123).into()
        } else {
            theme.glass_hover()
        };
        let hover_fg = gpui::white();
        let foreground = theme.text;
        let action = cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            match button {
                WindowButton::Minimize => window.minimize_window(),
                WindowButton::Maximize => window.zoom_window(),
                WindowButton::Close => {
                    if this.prepare_window_close(cx) {
                        window.remove_window();
                    }
                }
            }
        });
        div()
            .id(id)
            .w(px(LINUX_CAPTION_BUTTON_WIDTH))
            .h(px(LINUX_CAPTION_BUTTON_WIDTH))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .text_color(foreground)
            .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
            .active(move |style| style.bg(hover_bg.opacity(0.7)))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(action)
            .child(
                icon(icon_path)
                    .size(px(18.0))
                    .text_color(foreground)
                    .hover(move |style| style.text_color(hover_fg)),
            )
            .into_any_element()
    }

}
