//! Main content column: conversation, composer, status strip and right pane.

use super::*;

impl Shell {
    /// The composer widget strip: Changes / subagents / processes chips over
    /// the prompt, each reading data the shell already has. Hidden chips
    /// render nothing; an open popup holds its chip up.
    pub(super) fn render_widget_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::composer_widgets::{StripInput, WidgetKind, chip_allowed, chip_content};

        let chat_id = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|row| row.id.clone());
        self.widgets
            .reset_checked(chat_id.as_deref(), self.widgets_chat.as_deref());
        self.widgets_chat = chat_id.clone();
        let Some(_) = chat_id else {
            return div().into_any_element();
        };

        let theme = Theme::of(cx).clone();
        let harness = self
            .composer
            .read(cx)
            .pickers()
            .read(cx)
            .resolved(cx)
            .harness;
        let surface = harness.map_or_else(
            komet_proto::HarnessInteractionSurface::none,
            komet_harness::capabilities::interaction_surface,
        );
        let input = StripInput {
            widget_changes: self.widget_changes_summary(cx),
            subagent_rows: self.transcript.read(cx).subagent_rows(cx),
            process_entries: self.transcript.read(cx).agent_shell_log(),
            subagents_allowed: chip_allowed(
                surface.subagents,
                surface.tool_progress,
                WidgetKind::Subagents,
            ),
            processes_allowed: chip_allowed(
                surface.subagents,
                surface.tool_progress,
                WidgetKind::Processes,
            ),
        };

        let now = std::time::Instant::now();
        let visible = self.widgets.step(&input, now);
        if self.widgets_wake.is_none()
            && let Some(delay) = self.widgets.wake_in(now)
        {
            self.arm_widgets_wake(delay, cx);
        }

        let focus = self
            .widgets
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let open = self.widgets.open.get().copied();
        let closing = self.widgets.open.closing_since();
        let mut overlay: Option<(WidgetKind, AnyElement)> = open.map(|kind| {
            (
                kind,
                self.widget_popup(kind, &input, surface.tool_progress, &theme, &focus, cx),
            )
        });

        let mut row = div().w_full().flex().flex_row().items_center().gap(px(4.0));
        for (ix, kind) in [
            WidgetKind::Changes,
            WidgetKind::Subagents,
            WidgetKind::Processes,
        ]
        .into_iter()
        .enumerate()
        {
            if !visible[ix] {
                continue;
            }
            let content = chip_content(kind, &input);
            let chip = self.render_widget_chip(kind, &content, open == Some(kind), &theme, cx);
            let chip = if overlay.as_ref().is_some_and(|(owned, _)| *owned == kind) {
                let (_, element) = overlay.take().expect("matched just above");
                chip.child(popover::anchored_menu_above(
                    "widget-popup",
                    element,
                    closing,
                ))
            } else {
                chip
            };
            row = row.child(chip);
        }

        div()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .px(px(Theme::SPACE_LG))
            .pb(px(6.0))
            .child(row)
            .into_any_element()
    }

    fn render_widget_chip(
        &mut self,
        kind: crate::composer_widgets::WidgetKind,
        content: &crate::composer_widgets::ChipContent,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use crate::composer_widgets::WidgetKind;

        let id = match kind {
            WidgetKind::Changes => "widget-changes",
            WidgetKind::Subagents => "widget-subagents",
            WidgetKind::Processes => "widget-processes",
        };
        let has_menu = kind != WidgetKind::Changes;
        div()
            .id(id)
            .h(px(24.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .px(px(9.0))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .bg(if open {
                crate::theme::ink(0.10)
            } else {
                crate::theme::ink(0.04)
            })
            .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.07))))
            .cursor_pointer()
            .child(
                icon(content.icon)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(div().min_w_0().truncate().child(content.label.clone()))
            .when(has_menu, |el| {
                el.child(
                    icon(icons::ALT_ARROW_DOWN)
                        .size(px(12.0))
                        .text_color(theme.text_faint),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    if has_menu {
                        this.widgets
                            .open
                            .note_trigger_press_matching(|open| *open == kind);
                    }
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| match kind {
                WidgetKind::Changes => this.open_changes_widget(cx),
                _ => this.toggle_widgets(kind, cx),
            }))
    }

    fn widget_popup(
        &mut self,
        kind: crate::composer_widgets::WidgetKind,
        input: &crate::composer_widgets::StripInput,
        progress: komet_proto::ProgressSurface,
        theme: &Theme,
        focus: &gpui::FocusHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::composer_widgets::WidgetKind;

        let card = popover::popover_card(theme)
            .id("widget-popup-card")
            .w(px(340.0))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .track_focus(focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_widgets(cx);
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_widgets(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0));
        let card = match kind {
            WidgetKind::Subagents => input.subagent_rows.iter().cloned().fold(card, |card, row| {
                card.child(self.subagent_popup_row(row, theme, cx))
            }),
            WidgetKind::Processes => input
                .process_entries
                .iter()
                .cloned()
                .fold(card, |card, entry| {
                    card.child(self.process_popup_row(entry, progress, theme))
                }),
            WidgetKind::Changes => card,
        };
        card.into_any_element()
    }

    fn subagent_popup_row(
        &mut self,
        row: crate::transcript::SubagentRowData,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use komet_doc::SubagentStatus;

        let status = row
            .spawn
            .status
            .or(row.live.as_ref().map(|live| live.status));
        let tint = match status {
            Some(SubagentStatus::Running) => theme.busy,
            Some(SubagentStatus::Done) => theme.success,
            Some(SubagentStatus::Failed) => theme.danger,
            None => theme.text_muted,
        };
        let label = crate::subagent::subagent_status_label(
            row.spawn.status,
            row.live.as_ref(),
            row.spawn.duration_ms,
            row.spawn.tool_count,
        )
        .unwrap_or_default();
        let title = row.spawn.title.clone();
        let model = row.spawn.model.clone();
        let id = SharedString::from(format!("subagent-row-{}", row.spawn.parent_tool_use_id));
        div()
            .id(id)
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_subagent_tab(&row, cx);
                this.close_widgets(cx);
            }))
            .child(div().flex_none().size(px(6.0)).rounded(px(3.0)).bg(tint))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .when_some(model, |el, model| {
                        el.child(
                            div()
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted)
                                .child(model),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(tint)
                    .child(label),
            )
            .into_any_element()
    }

    fn process_popup_row(
        &mut self,
        entry: crate::transcript::AgentShellEntry,
        progress: komet_proto::ProgressSurface,
        theme: &Theme,
    ) -> AnyElement {
        let tint = crate::composer_widgets::process_tint(entry.status, theme);
        let (command, last) = crate::composer_widgets::process_row_lines(&entry);
        let note = crate::composer_widgets::no_output_note(entry.status, progress);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .flex()
            .items_start()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .mt(px(5.0))
                    .size(px(6.0))
                    .rounded(px(3.0))
                    .bg(tint),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .font_family("monospace")
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(command),
                    )
                    .child(match last {
                        Some(line) => div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme.text_muted)
                            .child(line),
                        None => div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(note),
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_main(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar). Settings never
        // underlaps: pad below the overlaid titlebar.
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let home_canvas = !has_selection;
        self.composer.update(cx, |composer, cx| {
            composer.set_home_canvas(home_canvas, cx);
        });
        let no_project = self.state.read(cx).no_project;
        // While the engine is still connecting the space list is simply empty —
        // show the new-session canvas, not "Add a project", which would read as
        // a wrong state rather than a pending one.
        let booting = matches!(self.state.read(cx).gate(), GatePhase::Loading);

        // Content outlet: selected chat → transcript; nothing selected → the
        // centered new-session canvas; no spaces at all → the onboarding card.
        //
        // The transcript is a cached gpui view: it re-renders only when its
        // entity — or one of its embedded views — is notified (gpui dirties
        // the ancestors of a notified view). It observes AppState, its list
        // notifies on scroll, the in-place editor notifies on caret blink, and
        // `motion::hover_frame` notifies the fade owners. A shell repaint —
        // hover, spinner, sync overlay — no longer rebuilds it.
        let outlet: AnyElement = if has_selection {
            self.transcript
                .clone()
                .cached(StyleRefinement::default().size_full())
                .into_any_element()
        } else if !has_spaces && !no_project && !booting {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .relative()
                        .w_full()
                        .max_w(px(1187.0))
                        .h(px(451.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(crate::blobatar::blobatar(
                            "onboarding-blobatar",
                            64.0,
                            Some(theme.text.opacity(0.85)),
                        ))
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a project to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A project is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a project")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // Keep destination and checkout selectors in one row; agent/model
            // selection stays in the composer's action bar.
            let pickers = self.composer.read(cx).pickers().clone();
            let (target_selectors, checkout_selectors) = pickers.update(cx, |p, cx| {
                (p.render_target_selectors(cx), p.render_footer(window, cx))
            });
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .w_full()
                        .max_w(px(768.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .mb(px(28.0))
                                .text_size(px(20.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("What are we building?"),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .w_full()
                                        .px(px(Theme::SPACE_LG))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(Theme::SPACE_MD))
                                        .child(div().flex_none().child(target_selectors))
                                        .when_some(checkout_selectors, |el, controls| {
                                            el.child(div().flex_1().min_w_0().child(controls))
                                        }),
                                )
                                .child(div().h(px(12.0)).flex_none())
                                .child(self.render_widget_strip(cx))
                                .child(self.composer.clone()),
                        ),
                ))
                .into_any_element()
        };
        let status = self.render_status_strip(window, cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop files or images to attach" veil; a drop
        // stages them in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        let provider_switch_notice = self
            .composer
            .read(cx)
            .pickers()
            .read(cx)
            .provider_switch_notice()
            .map(|notice| {
                let theme = Theme::of(cx).clone();
                let (from_icon, from_tint) = crate::pickers::harness_brand_icon(notice.from);
                let (to_icon, to_tint) = crate::pickers::harness_brand_icon(notice.to);
                let transition = div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .child(
                        icon(from_icon)
                            .size(px(18.0))
                            .text_color(from_tint.unwrap_or(theme.text_muted)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(notice.from.display_name()),
                    )
                    .child(
                        icon(crate::icons::ARROW_RIGHT)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.75)),
                    )
                    .child(
                        icon(to_icon)
                            .size(px(18.0))
                            .text_color(to_tint.unwrap_or(theme.text)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(notice.to.display_name()),
                    );
                let card = div()
                    .px(px(16.0))
                    .py(px(11.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(crate::theme::hairline(0.12))
                    .bg(theme.surface)
                    .shadow(crate::theme::card_selected_shadows())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(5.0))
                    .child(transition)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_muted.opacity(0.8))
                            .child(format!(
                                "{} is selected for this session",
                                notice.to.display_name()
                            )),
                    );
                div()
                    .absolute()
                    .top(px(24.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(motion::fade_in(
                        ("provider-switch-notice", notice.sequence),
                        card,
                    ))
                    .into_any_element()
            });
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, false, cx));
                cx.notify();
            }))
            .on_drag_move::<WorkspacePathDrag>(cx.listener(
                |this, e: &gpui::DragMoveEvent<WorkspacePathDrag>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop::<WorkspacePathDrag>(cx.listener(
                |this, payload: &WorkspacePathDrag, window, cx| {
                    this.file_drag_active = false;
                    this.composer.update(cx, |composer, cx| {
                        composer.add_workspace_path(&payload.path, payload.is_directory, window, cx)
                    });
                    cx.notify();
                },
            ))
            .children(provider_switch_notice)
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The terminal dock is NOT glass the transcript may slide
                    // under: with the dock's translucent fill, transcript text
                    // ghosted through the grid (user report). The underlay
                    // ends at the dock's top instead, riding the same height
                    // tween the dock animates with; `stack_h` below is only
                    // the chrome that still overlaps the transcript (status
                    // strip + composer).
                    let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                    let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .bottom(px(term_h))
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                true,
                                true,
                                div().size_full().child(outlet),
                            )
                            .inset_top(0.0)
                            .band_top(Theme::TRANSCRIPT_FADE_BAND)
                            .band_bottom(bottom_band),
                        )
                        .children(self.render_jump_to_bottom(stack_h, cx))
                },
            )
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts), composer, terminal dock. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .when(has_selection, |el| el.child(status))
                    .when(has_spaces && has_selection, |el| {
                        el.when_some(self.render_plan_card(cx), |el, card| el.child(card))
                            .child(self.render_widget_strip(cx))
                            .child(self.composer.clone())
                    })
                    .child(self.render_terminal_container(cx))
            })
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop files or images to attach"),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    /// `stack_h` is the measured bottom chrome stack the full-height
    /// transcript scrolls under — the pill anchors just above it (the -14
    /// carries the old status-strip overlap).
    fn render_jump_to_bottom(
        &mut self,
        stack_h: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .bottom(px(stack_h - 14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(motion::dialog_in(
                    "jump-to-bottom",
                    div()
                        .id("jump-to-bottom-btn")
                        .h(px(30.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(11.0))
                        .pr(px(13.0))
                        .cursor_pointer()
                        // Hover must BRIGHTEN the opaque pill, never replace it
                        // with a translucent wash (a 10%-alpha bg here made the
                        // pill go see-through on hover — user-reported), and it
                        // fades over the CSS transition-colors 150ms, not snaps.
                        .bg(motion::hover_blend(
                            "jump-pill",
                            theme.surface_raised,
                            theme.surface_raised_hover,
                        ))
                        .on_hover(motion::hover_listener("jump-pill", cx.entity_id()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcript
                                .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        }))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("↓")),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from("Scroll to bottom")),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its 5px read as
        // dead air between the seam and the tab bar (user report).
        let inner = div()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(panel))
            .child(handle.absolute().top_0().left_0().right_0());

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The selected chat's current plan, above the composer. Visible while
    /// work remains or the run is still live: once every task is done and the
    /// run has settled, the transcript chips keep the record and the card
    /// leaves the chrome. Width matches the composer column so the two read as
    /// one stack; nothing here takes focus or blocks typing.
    fn render_plan_card(&self, cx: &App) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let chat_id = state.selected_chat.as_deref()?;
        let plan = state.active_plan.as_deref()?;
        let total = plan.len();
        let done = plan.iter().filter(|item| item.done).count();
        let run_live = matches!(
            state.indicator_for(chat_id, Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        );
        let (rows, hidden) = plan_card_layout(total, done, run_live)?;
        let theme = Theme::of(cx).clone();
        Some(
            div()
                .w_full()
                .max_w(px(768.0))
                .mx_auto()
                .px(px(Theme::SPACE_LG))
                .pb(px(Theme::SPACE_SM))
                .child(
                    div()
                        .id("active-plan")
                        .w_full()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_raised)
                        .px(px(Theme::SPACE_MD))
                        .py(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            icon(icons::CHECKLIST)
                                                .size(px(13.0))
                                                .text_color(theme.text_muted),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child("Plan"),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(format!("{done}/{total}"))),
                                ),
                        )
                        .children(plan.iter().take(rows).map(|item| {
                            div()
                                .flex()
                                .items_start()
                                .gap(px(6.0))
                                .text_size(px(12.0))
                                .child(
                                    div()
                                        .w(px(14.0))
                                        .flex_none()
                                        .text_color(if item.done {
                                            theme.success
                                        } else {
                                            theme.text_faint
                                        })
                                        .child(if item.done { "✓" } else { "○" }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_color(if item.done {
                                            theme.text_muted.opacity(0.7)
                                        } else {
                                            theme.text
                                        })
                                        .child(SharedString::from(item.text.clone())),
                                )
                        }))
                        .when(hidden > 0, |el| {
                            el.child(
                                div()
                                    .pl(px(20.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(format!("+{hidden} more"))),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (komet's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(px(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(
                    crate::thinking_orbs::ThinkingOrb::new(
                        crate::thinking_orbs::OrbState::Listening,
                        20.0,
                    )
                    .speed(1.25)
                    .driven(cx.entity_id(), window, cx),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right pane — the surface host (t3code RightPanelTabs): hidden by
    /// default, drag-resizable. Content is the ACTIVE surface — the Diff
    /// page (its options row + the lazy [`Changes`] viewer), an embedded
    /// terminal, or the "Open a surface" picker when no tabs exist.
    pub(super) fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let active = self.resolved_right_active(cx);
            let main: AnyElement = match active {
                RightSurface::Files => {
                    let key = self.panel_key(cx);
                    if let Some(files) = self.files.get(&key).cloned() {
                        files.update(cx, |files, cx| files.ensure_loaded(cx));
                        files.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::File(id) => {
                    if let Some(file) = self.file_surfaces.get(&id).cloned() {
                        file.update(cx, |file, cx| file.ensure_loaded(cx));
                        file.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::Diff(id) if self.diffs.contains_key(&id) => {
                    let changes = self.diffs.get(&id).cloned().expect("checked");
                    // Idempotent — also covers a persisted-open pane on boot.
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                    // The diff options (scope dropdown, ref selector,
                    // fold-all) moved DOWN from the titlebar band — the
                    // surface tabs own that row now; the expand/close
                    // buttons stayed up there (user request).
                    let controls =
                        changes.update(cx, |changes, cx| changes.render_header_controls(cx));
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(px(36.0))
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(controls),
                        )
                        // Cached boundary: the diff surface observes AppState
                        // and its own list state, and gpui dirties this view
                        // when an embedded view (history, fetch button, ref
                        // search) is notified. Resizes invalidate via the
                        // cached bounds.
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .child(changes.cached(StyleRefinement::default().size_full())),
                        )
                        .into_any_element()
                }
                RightSurface::Terminal(tab) => {
                    let panel = self.right_terminal_panel(cx);
                    // Keep the embedded panel's own active tab aligned with
                    // the resolved surface (fallbacks can move it).
                    panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
                    panel.into_any_element()
                }
                RightSurface::Subagent(id) => {
                    if let Some(panel) = self.subagent_tabs.get(&id).cloned() {
                        // Idempotent, like the diff surface: a persisted-open
                        // pane on boot starts watching here, not at tab open.
                        panel.update(cx, |panel, cx| panel.ensure_started(cx));
                        panel.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                _ => self.render_surface_picker(cx),
            };
            // Files browser companion: shown alongside File editor tabs
            // and any other surface that isn't Files itself, matching
            // VS Code's explorer-stays-put behaviour.
            let files_panel = if active == RightSurface::Files {
                None
            } else {
                let key = self.panel_key(cx);
                self.right_tabs.get(&key).and_then(|tabs| {
                    if tabs.contains(&RightSurface::Files) {
                        self.files.get(&key).cloned()
                    } else {
                        None
                    }
                })
            };
            match files_panel {
                Some(files) => {
                    // Two independent-looking panels (own bg/border/rounded
                    // corners each, separated by a gap) rather than one
                    // merged block split by a hairline — user request: the
                    // file editor should read as its own container, not as
                    // fused into the explorer's box.
                    div()
                        .flex()
                        .flex_row()
                        .size_full()
                        .gap(px(8.0))
                        .p(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .rounded(px(8.0))
                                .overflow_hidden()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .child(main),
                        )
                        .child(
                            div()
                                .w(px(240.0))
                                .flex_none()
                                .h_full()
                                .rounded(px(8.0))
                                .overflow_hidden()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.glass())
                                .child(files),
                        )
                        .into_any_element()
                }
                None => main,
            }
        } else {
            gpui::Empty.into_any_element()
        };
        // Flush panel (user request — the inset card is gone): full window
        // height with a left hairline, glass-friendly like the terminal dock
        // (translucent over the frost; solid otherwise). The resize grabber
        // floats over the border seam.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the panel's left border.
            .left(px(0.0));
        let panel_bg = bg;
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            // In takeover the panel's left edge IS the sidebar seam, which
            // already carries the sidebar tone's right hairline — a second
            // border there doubled up (user report).
            .when(!self.right_pane_expanded, |el| {
                el.border_l_1().border_color(theme.border)
            })
            .bg(panel_bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        // Takeover mode has no drag width — the handle would fight the
        // viewport-derived target.
        let handle = (!self.right_pane_expanded).then_some(handle);
        self.pane_container(
            self.right_tween,
            target,
            div()
                .h_full()
                .relative()
                .child(panel)
                .children(handle)
                .into_any_element(),
        )
    }

    /// The right pane's empty state: the "Open a surface" heading over a
    /// compact vertical list of surface rows (icon + label) — the Capy
    /// arrangement (user request): the old two-card grid clipped in narrow
    /// panes and wasted short ones.
    fn render_surface_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let text = theme.text;
        let muted = theme.text_muted;
        let border = theme.border;
        let border_strong = theme.border_strong;
        let row = |id: &'static str, icon_path: &'static str, title: &'static str| {
            div()
                .id(id)
                .w_full()
                .h(px(44.0))
                .px(px(14.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(border)
                .bg(crate::theme::ink(0.02))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(move |s| s.bg(crate::theme::ink(0.05)).border_color(border_strong))
                .child(icon(icon_path).size(px(15.0)).flex_none().text_color(muted))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text)
                        .child(SharedString::from(title)),
                )
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(280.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_center()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text)
                            .child(SharedString::from("Open a surface")),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_center()
                            .text_size(px(11.5))
                            .text_color(muted)
                            .child(SharedString::from(
                                "Choose what to show in the right panel.",
                            )),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                row("surface-card-terminal", icons::TERMINAL, "Terminal").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.add_terminal_surface(cx);
                                    }),
                                ),
                            )
                            .child(
                                row("surface-card-files", icons::FOLDER_WITH_FILES, "Files")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_files_surface(window, cx);
                                    })),
                            )
                            // Git only where there IS git — the pane itself
                            // no longer gates on it (terminals work anywhere).
                            .when(self.space_git_detected(cx), |el| {
                                el.child(
                                    row("surface-card-git", icons::GIT_BRANCH, "Git").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.add_diff_surface(cx);
                                        }),
                                    ),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn close_right_plus(&mut self, cx: &mut Context<Self>) {
        if self.right_plus.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.right_plus);
        }
        cx.notify();
    }

    /// The titlebar strip over the right pane: one chip per surface tab
    /// (icon · title · ✕) plus the `+` menu — the t3code RightPanelTabs bar,
    /// living in the top row; the diff options moved into the pane below.
    pub(crate) fn render_right_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        /// Fixed chip slot — the terminal drawer's drag mechanics (drop-index
        /// quantisation + slide offsets) assume uniform widths.
        const CHIP_W: f32 = 112.0;
        const CHIP_SLOT: f32 = CHIP_W + 4.0; // + the strip's own gap

        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the strip.
        if self.right_tab_drag.is_some() && !cx.has_active_drag() {
            self.right_tab_drag = None;
        }
        let rows = self.right_surface_rows(cx);
        let count = rows.len();
        let active = self.resolved_right_active(cx);
        let drag = self
            .right_tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        // Fade flags from the LAST frame's scroll state (invisible lag).
        // The EdgeFade scope below fades per-pixel on x for glyphs AND
        // quads/images (fork 5d1f83d) — washes dissolve across the band.
        const FADE_WIDTH: f32 = 36.0;
        let scrolled = -f32::from(self.right_tab_scroll.offset().x);
        let max_scroll = f32::from(self.right_tab_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        // The old session-tab strip's proven scroll shape: the flex row IS
        // the scroller (id + overflow_x_scroll + track_scroll), wrapped in a
        // relative min_w_0 region below; drop math runs in CONTENT
        // coordinates (viewport-relative x plus the scrolled-off width).
        let scroll_for_drag = self.right_tab_scroll.clone();
        let mut strip = div()
            .id("right-surface-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.right_tab_scroll)
            .on_drag_move::<RightTabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<RightTabDrag>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.panel_key != this.panel_key(cx) {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x)
                        - f32::from(event.bounds.left())
                        - f32::from(scroll_for_drag.offset().x);
                    let over = crate::terminal::panel::drop_index(rel_x, CHIP_SLOT, count);
                    this.update_right_tab_drag_over(from, over, cx);
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(move |this, payload: &RightTabDrag, _, cx| {
                if payload.panel_key != this.panel_key(cx) {
                    this.right_tab_drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .right_tab_drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from);
                this.right_tab_drag = None;
                this.reorder_right_tabs(payload.from, to, cx);
            }));
        for (ix, (surface, title, _dirty, _detail)) in rows.into_iter().enumerate() {
            let is_active = surface == active;
            let icon_path = match surface {
                RightSurface::Files => icons::FOLDER_WITH_FILES,
                RightSurface::File(_) => icons::DOCUMENT,
                RightSurface::Diff(_) => icons::GIT_BRANCH,
                RightSurface::Subagent(_) => icons::DOCUMENT,
                _ => icons::TERMINAL,
            };
            // t3 tab hover: the surface icon swaps IN PLACE for the close ✕
            // (same slot, no width jump) — the ✕ only shows while the tab is
            // hovered (user request).
            let group: SharedString = format!("right-surface-tab-{ix}").into();
            let ghost_title = title.clone();
            let workspace_path = self.workspace_path_for_surface(surface, cx);
            let chip = div()
                .id(("right-surface-tab", ix))
                .group(group.clone())
                .h(px(34.0))
                .w(px(CHIP_W))
                .flex_none()
                .pl(px(8.0))
                .pr(px(4.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.0))
                .cursor_pointer()
                .border_b_2()
                .border_color(if is_active {
                    theme.accent
                } else {
                    gpui::transparent_black()
                })
                // The old session-tab strip's solved carve-out: NOT
                // `.occlude()` — a BlockMouse hitbox ends the hit test,
                // so the scroll container behind the tabs never saw
                // wheel events and an overflowing strip could not be
                // scrolled (tabs tile the whole region). ExceptScroll
                // keeps the titlebar drag-region carve-out and lets the
                // strip scroll.
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .when(is_active, |el| el.bg(crate::theme::wash(0.06)))
                .when(!is_active, |el| {
                    el.hover(|s| s.bg(crate::theme::wash(0.04)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_right_active(surface, cx);
                }))
                // Middle-click closes, like every tab strip.
                .on_mouse_down(
                    gpui::MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        this.close_right_surface(surface, window, cx);
                    }),
                )
                .on_drag(
                    RightTabDrag {
                        panel_key: self.panel_key(cx),
                        from: ix,
                        title: ghost_title,
                        workspace_path,
                    },
                    |payload, _point, _, cx| {
                        let title = payload.title.clone();
                        cx.stop_propagation();
                        cx.new(|_| SurfaceTabGhost { title })
                    },
                )
                .child(icon(icon_path).size(px(13.0)).text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted.opacity(0.7)
                }))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(if is_active {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(title),
                )
                .child(
                    div()
                        .id(("right-surface-close", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(3.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.0)
                        .group_hover(group.clone(), |s| s.opacity(1.0))
                        .hover(|s| s.bg(crate::theme::wash(0.12)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_right_surface(surface, window, cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        ),
                );
            // Sliding transform while a sibling drags over (the terminal
            // drawer's exact recipe): animate 150ms between committed
            // offsets; the dragged tab leaves an invisible spacer — the
            // ghost carries it.
            let wrapped: AnyElement = match drag {
                Some((from, over, epoch, prev_over)) if ix != from => {
                    let target = crate::terminal::panel::slide_offset(ix, from, over) * CHIP_SLOT;
                    let start =
                        crate::terminal::panel::slide_offset(ix, from, prev_over) * CHIP_SLOT;
                    div()
                        .relative()
                        .child(chip.with_animation(
                            ("right-tab-slide", (ix as u64) | ((epoch as u64) << 32)),
                            TAB_SLIDE.animation(),
                            move |el, t| el.left(px(motion::lerp(start, target, t))),
                        ))
                        .into_any_element()
                }
                Some((from, ..)) if ix == from => div()
                    .w(px(CHIP_W))
                    .h(px(24.0))
                    .flex_none()
                    .into_any_element(),
                _ => chip.into_any_element(),
            };
            strip = strip.child(wrapped);
        }
        // The `+` — a small menu offering the two surfaces (t3 "Add panel
        // surface"); mirrors the picker cards.
        let plus_open = self.right_plus.get().is_some();
        let plus_fade = "right-surface-add-fade";
        let mut plus = div()
            .id("right-surface-add")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                plus_fade,
                crate::theme::wash(0.0),
                crate::theme::wash(0.11),
            ))
            .on_hover(motion::hover_listener(plus_fade, cx.entity_id()))
            .block_mouse_except_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.right_plus.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.right_plus.take_press_was_open() {
                    this.close_right_plus(cx);
                } else {
                    this.right_plus.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        if plus_open {
            let closing = self.right_plus.closing_since();
            let menu = popover::popover_card(&theme)
                .w(px(168.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_right_plus(cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            popover::menu_row(&theme, false, "right-plus-terminal", cx.entity_id())
                                .id("right-plus-terminal-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_terminal_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::TERMINAL)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Terminal")),
                        )
                        .child(
                            popover::menu_row(&theme, false, "right-plus-files", cx.entity_id())
                                .id("right-plus-files-row")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_files_surface(window, cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::FOLDER_WITH_FILES)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Files")),
                        )
                        .child(
                            popover::menu_row(&theme, false, "right-plus-diff", cx.entity_id())
                                .id("right-plus-diff-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_diff_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                // "Git", not "Git diff" — the surface hosts
                                // history and per-commit views too (user
                                // request; matches the picker card).
                                .child(SharedString::from("Git")),
                        ),
                )
                .into_any_element();
            plus = plus.relative().child(popover::anchored_menu_below_gap(
                "right-plus-menu",
                menu,
                closing,
                10.0,
            ));
        }
        strip = strip.child(plus);
        // Edge fades on whichever side hides tabs (flags computed above).
        // Glass: per-glyph EdgeFade scope over the chips' own opacity ramps;
        // opaque: painted gradients in the shell surface tone.
        let glass = theme.is_glass();
        let bar_bg = theme.surface;
        let region = div()
            .relative()
            .min_w_0()
            .size_full()
            .flex()
            .items_center()
            .child(strip)
            .when(fade_left && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });
        if glass {
            crate::edge_fade::edge_faded(FADE_WIDTH, false, false, region)
                .fade_left(fade_left)
                .fade_right(fade_right)
                .into_any_element()
        } else {
            region.into_any_element()
        }
    }

    /// Toggle the changes-panel takeover (the header's expand button, t3code
    /// parity): the panel grows to fill everything right of the sidebar,
    /// hiding the conversation column; toggling back restores the saved
    /// width. Rides the same width tween as open/close so the jump glides.
    pub(super) fn toggle_right_pane_expand(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target(cx);
        self.right_pane_expanded = !self.right_pane_expanded;
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        cx.notify();
    }
}

/// Row ceiling for the plan card above the composer: the card must stay a
/// glance, not a second transcript.
pub const PLAN_CARD_MAX_ROWS: usize = 6;

/// The plan card's two rules, decided apart from rendering. Visibility: open
/// work always shows the card, and a fully-done plan leaves once the run
/// settles (the transcript chips keep the record) — a run still going keeps the
/// card up so the last tick is never a flicker. Cap: rows past
/// [`PLAN_CARD_MAX_ROWS`] are counted so the card can announce `+N` instead of
/// silently cutting the list (transparency invariant I9).
pub fn plan_card_layout(total: usize, done: usize, run_live: bool) -> Option<(usize, usize)> {
    if total == 0 || (done == total && !run_live) {
        return None;
    }
    let rows = total.min(PLAN_CARD_MAX_ROWS);
    Some((rows, total - rows))
}

/// A size-6 icon button for the titlebar strip (komet window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
pub fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // komet window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key, owner))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(17.0)).text_color(muted))
}

pub const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
pub const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;
pub const LINUX_CAPTION_BUTTON_WIDTH: f32 = 34.0;

pub fn linux_control_side_width(
    buttons: [Option<WindowButton>; 3],
    has_minimize: bool,
    has_maximize: bool,
) -> f32 {
    let count = buttons
        .iter()
        .flatten()
        .filter(|button| match button {
            WindowButton::Minimize => has_minimize,
            WindowButton::Maximize => has_maximize,
            WindowButton::Close => true,
        })
        .count();
    if count == 0 {
        0.0
    } else {
        count as f32 * LINUX_CAPTION_BUTTON_WIDTH + (count - 1) as f32 * 2.0 + 8.0
    }
}

pub fn effective_controls(
    buttons: [Option<WindowButton>; 3],
    controls: gpui::WindowControls,
) -> (bool, bool) {
    // If the platform/compositor advertises controls (or on X11), respect them.
    // However, on Wayland compositors without xdg-decoration / WmCapabilities (e.g. GNOME Mutter),
    // controls.minimize and controls.maximize default to false even though the desktop
    // button-layout expects them. If the button layout explicitly includes minimize/maximize,
    // allow them unless controls explicitly signaled support and excluded them.
    let has_layout_min = buttons.contains(&Some(WindowButton::Minimize));
    let has_layout_max = buttons.contains(&Some(WindowButton::Maximize));
    (
        controls.minimize || has_layout_min,
        controls.maximize || has_layout_max,
    )
}

pub fn linux_button_layout(cx: &App, controls: gpui::WindowControls) -> WindowButtonLayout {
    effective_linux_button_layout(
        cx.button_layout().unwrap_or(WindowButtonLayout {
            left: [None; 3],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ],
        }),
        controls,
    )
}

pub fn effective_linux_button_layout(
    layout: WindowButtonLayout,
    controls: gpui::WindowControls,
) -> WindowButtonLayout {
    let (left_min, left_max) = effective_controls(layout.left, controls);
    let (right_min, right_max) = effective_controls(layout.right, controls);
    if linux_control_side_width(layout.left, left_min, left_max)
        + linux_control_side_width(layout.right, right_min, right_max)
        > 0.0
    {
        layout
    } else {
        WindowButtonLayout {
            left: [None; 3],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ],
        }
    }
}

pub fn titlebar_right_padding(is_windows: bool, linux_controls: f32, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else {
        linux_controls
    }
}

pub fn client_resize_edge(
    position: Point<Pixels>,
    size: Size<Pixels>,
    inset: f32,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let x = f32::from(position.x);
    let y = f32::from(position.y);
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    let left = x < inset && !tiling.left;
    let right = x >= width - inset && !tiling.right;
    let top = y < inset && !tiling.top;
    let bottom = y >= height - inset && !tiling.bottom;

    match (top, right, bottom, left) {
        (true, false, false, true) => Some(ResizeEdge::TopLeft),
        (true, true, false, false) => Some(ResizeEdge::TopRight),
        (false, true, true, false) => Some(ResizeEdge::BottomRight),
        (false, false, true, true) => Some(ResizeEdge::BottomLeft),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, true, _) => Some(ResizeEdge::Bottom),
        (_, _, _, true) => Some(ResizeEdge::Left),
        _ => None,
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
pub fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A titlebar history button (komet window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
pub fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, owner, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (komet __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
pub fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // komet __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key, owner))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}
