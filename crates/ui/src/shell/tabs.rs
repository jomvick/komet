//! Session navigation — the horizontal tab strip is gone (wing 2026-08-10):
//! the activity sidebar IS the session list, and the titlebar names the
//! selected session (harness brand icon + title). When the sidebar is
//! collapsed, a `+` new-session button fades into the titlebar's left end
//! (riding the sidebar width tween). `UiSettings.open_tabs` is legacy — no
//! longer read or written.

use super::*;

impl Shell {
    /// Boot landing: the most recently active visible chat once the first
    /// chats frame has synced (manual selection wins; no chats → the
    /// new-session canvas shows).
    pub(super) fn boot_select_chat(&mut self, cx: &mut Context<Self>) {
        let first = {
            let state = self.state.read(cx);
            if !state.chats_synced || state.selected_chat.is_some() || state.auto_selected {
                return;
            }
            state
                .overview_chats(Utc::now())
                .first()
                .map(|(_, c)| c.id.clone())
        };
        if let Some(first) = first {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(first), cx));
        }
    }

    /// Open a session from the sidebar: select it, the main area follows.
    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    /// `+` (sidebar header, or the titlebar while the sidebar is collapsed):
    /// open the new-session canvas. A set sidebar filter re-homes the canvas
    /// onto that project; under "All" the current pick (the last selected
    /// project, restored from composer defaults) stands.
    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        let target = {
            let state = self.state.read(cx);
            self.settings
                .space_filter
                .clone()
                .filter(|id| state.space_row(id).is_some())
        };
        self.state.update(cx, |s, cx| {
            if target.is_some() {
                s.select_space(target, cx);
            }
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    /// The unified titlebar in chat mode:
    /// `[fading +] [harness icon + session title] … [toggle-changes]`.
    /// Replaces the tab strip; inherits its titlebar duties (drag region,
    /// animated left inset, the toggle-changes button on git projects).
    pub(super) fn render_session_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (title, harness, on_canvas): (SharedString, Option<komet_proto::HarnessId>, bool) = {
            let state = self.state.read(cx);
            match state.selected_chat_row() {
                Some(chat) => (
                    SharedString::from(transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )),
                    chat.config.as_ref().map(|c| c.harness),
                    false,
                ),
                None => (SharedString::from("New session"), None, true),
            }
        };
        let git = self.space_git_detected(cx);

        // The new-session `+` renders in the WINDOW-CONTROL CLUSTER while the
        // sidebar is collapsed (`render_titlebar_cluster`) — this row only
        // budgets for it: the title's left inset grows by one button slot as
        // the + fades in, so the text never sits under it.
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let plus_inset = 26.0 * self.titlebar_plus_alpha();

        // Same glide as the old strip: content starts at the inset card's
        // left edge while the sidebar is open, and slides toward the control
        // cluster as it collapses.
        let content_left =
            (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start() + plus_inset);
        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(8.0))
            .pl(px(content_left))
            .pr(px(titlebar_right_padding(
                cfg!(target_os = "windows"),
                Theme::SPACE_LG,
            )))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(theme.text_muted)),
                            )
                        },
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if on_canvas {
                                theme.text_muted.opacity(0.7)
                            } else {
                                theme.text.opacity(0.85)
                            })
                            .child(title),
                    ),
            )
            .child(div().flex_1())
            // Stable location: the toggle shows whether the pane is open or
            // not (the pane's own header is gone).
            .when(git, |el| {
                el.child(header_icon_button(
                    "toggle-changes",
                    icons::SIDEBAR_MINIMALISTIC,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                ))
            });

        // The unified window titlebar: full-width on the glass shell, ABOVE
        // the inset card. No bottom border — the card's own hairline is the
        // separation; the glass gutter shows between.
        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-titlebar", bar, cx)
            .into_any_element()
    }
}
