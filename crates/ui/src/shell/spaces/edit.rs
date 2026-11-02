//! Space context menu plus the rename/delete dialogs.

use super::*;

impl Shell {
    // ---- space context menu / rename / delete overlays ----

    pub(crate) fn close_space_menu(&mut self, cx: &mut Context<Self>) {
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.space_menu);
            cx.notify();
        }
    }

    pub(crate) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.close_space_menu(cx);
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Project name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(crate) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.as_ref() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            // Empty names aren't valid — keep the dialog open so a cleared
            // field doesn't silently close with no rename and no feedback.
            cx.notify();
            return;
        }
        let space_id = dialog.space_id.clone();
        self.rename_space_dialog = None;
        self.mutate(
            serde_json::json!({ "op": "renameSpace", "spaceId": space_id, "name": name }),
            cx,
        );
        cx.notify();
    }

    pub(crate) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(crate) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.get().cloned() {
            let closing = self.space_menu.closing_since();
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_space_menu(cx);
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(
                        &theme,
                        false,
                        format!("space-menu-rename-{space_id}"),
                        cx.entity_id(),
                    )
                    .id("space-menu-rename")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_rename_space(rename_id.clone(), cx)
                    }))
                    .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                    .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(
                        &theme,
                        false,
                        format!("space-menu-delete-{space_id}"),
                        cx.entity_id(),
                    )
                    .id("space-menu-delete")
                    .text_color(theme.danger)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_space_menu(cx);
                        this.delete_space_confirm = Some(delete_id.clone());
                        this.space_delete_focus_pending = true;
                        cx.notify();
                    }))
                    .child(
                        icon(icons::TRASH_BIN_MINIMALISTIC)
                            .size(px(16.0))
                            .text_color(theme.danger),
                    )
                    .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at(
                "space-context-menu",
                position,
                menu,
                closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename project"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(
                                &theme,
                                "Cancel",
                                "rename-space-cancel",
                                cx.entity_id(),
                            )
                            .id("rename-space-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_space_dialog = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            if std::mem::take(&mut self.space_delete_focus_pending) {
                window.focus(&self.space_delete_focus, cx);
            }
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this project".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .track_focus(&self.space_delete_focus)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.delete_space_confirm = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Remove project?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(
                                &theme,
                                "Cancel",
                                "delete-space-cancel",
                                cx.entity_id(),
                            )
                            .id("delete-space-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_space_confirm = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}
