//! Sessions sidebar rendering: filter header, space menu, grouped rows, archive section.

use super::*;

impl Shell {
    /// The sidebar's Project tree header, project filter, and new-session
    /// control. Sits outside the scroll region so the filter dropdown never clips.
    pub(crate) fn render_spaces_filter(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filter = self.settings.space_filter.clone();
        // Name + the dropdown rows' "@ device" tag on the trigger itself, so
        // the filtered space's host reads without opening the picker.
        let (label, device_tag): (SharedString, Option<(SharedString, bool)>) = {
            let state = self.state.read(cx);
            match filter.as_deref().and_then(|id| state.space_row(id)) {
                Some(space) => {
                    let (tag, offline) = state.space_device_tag(space, Utc::now());
                    (
                        space.display_name().to_string().into(),
                        Some((tag.into(), offline)),
                    )
                }
                None => (SharedString::from("Project"), None),
            }
        };
        let open = self.spaces_menu.is_open();

        let trigger = div()
            .id("spaces-filter")
            .flex_1()
            .min_w_0()
            .h(px(29.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                "spaces-filter",
                theme.text.opacity(0.8),
                theme.text,
            ))
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "spaces-filter",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                motion::set_hover(
                    "spaces-filter",
                    cx.entity_id(),
                    *hovered,
                    motion::reduced_motion(cx),
                );
                cx.notify();
                if *hovered && !this.spaces_menu.is_open() {
                    this.open_spaces_menu(window, cx);
                }
            }))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.spaces_menu.is_open() {
                    this.open_spaces_menu(window, cx);
                }
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            // flex_1 pushes the caret to the trigger's right edge and gives
            // long space names a bound to truncate against; the "@ device"
            // tag hugs the name inside it rather than sitting by the caret.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().min_w_0().truncate().child(label))
                    .when_some(device_tag, |el, (tag, offline)| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::NORMAL)
                                .text_color(theme.text_muted.opacity(0.45))
                                .child(tag),
                        )
                        // Disconnected glyph, not the word (user request).
                        .when(offline, |el| {
                            el.child(
                                icon(icons::WIFI_OFF)
                                    .size(px(12.0))
                                    .flex_none()
                                    .text_color(theme.warning.opacity(0.8)),
                            )
                        })
                    }),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            );
        let trigger = if filter.is_some() && self.spaces_menu.get().is_some() {
            let closing = self.spaces_menu.closing_since();
            let menu = self.render_spaces_menu(theme, cx);
            trigger
                .relative()
                .child(popover::anchored_menu_below_unfrosted(
                    "spaces-filter-menu",
                    menu,
                    closing,
                ))
        } else {
            trigger
        };
        let trigger = if filter.is_none() {
            let expanded = self.settings.project_tree_expanded;
            let project_count = self.state.read(cx).spaces.len();
            let project_header = div()
                .id("project-tree-toggle")
                .flex_1()
                .min_w_0()
                .h(px(29.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .rounded(px(8.0))
                .px(px(10.0))
                .cursor_pointer()
                .bg(motion::hover_blend(
                    "project-tree-toggle",
                    crate::theme::wash(0.0),
                    crate::theme::wash(0.14),
                ))
                .on_hover(motion::hover_listener(
                    "project-tree-toggle",
                    cx.entity_id(),
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings.project_tree_expanded = !this.settings.project_tree_expanded;
                    this.schedule_save(cx);
                    cx.notify();
                }))
                .child(
                    icon(icons::FOLDER_WITH_FILES)
                        .size(px(15.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text.opacity(0.85))
                        .child(SharedString::from("Project")),
                )
                .when(!expanded && project_count > 0, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.5))
                            .child(SharedString::from(project_count.to_string())),
                    )
                })
                .child(
                    icon(if expanded {
                        icons::ALT_ARROW_DOWN
                    } else {
                        icons::ALT_ARROW_RIGHT
                    })
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.65)),
                );

            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(project_header)
                .into_any_element()
        } else {
            trigger.into_any_element()
        };

        // NEW SESSION beside the trigger (adding a project lives in the
        // dropdown's "New project…" row now). While the sidebar is collapsed
        // this same button fades into the titlebar instead
        // (`render_session_title_bar`). A plain button — the canvas showing
        // is not an "active" state worth a selected wash (user feedback).
        let add = div()
            .id("sidebar-new-session")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "sidebar-new-session",
                crate::theme::wash(0.0),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener(
                "sidebar-new-session",
                cx.entity_id(),
            ))
            .on_click(cx.listener(|this, _, _, cx| this.open_new_session(cx)))
            .child(
                icon(icons::PLUS)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(trigger)
            .child(add)
            .into_any_element()
    }

    /// The dropdown card: search on top, "All projects" + space rows (check on
    /// the active filter; right-click for rename/remove) + "New project…".
    pub(crate) fn render_spaces_menu(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (search, active, focus, list_scroll, page) = {
            let Some(menu) = self.spaces_menu.get() else {
                return div().into_any_element();
            };
            (
                menu.search.clone(),
                menu.active,
                menu.focus.clone(),
                menu.list_scroll.clone(),
                menu.page,
            )
        };
        let context_space = self.context_space(cx);
        let mut actions = popover::popover_card(theme).w(px(220.0)).flex().flex_col();

        let (worktrees_enabled, settings_enabled) = self.project_menu_action_availability(cx);
        actions = actions.child(
            self.project_menu_action_row(
                "project-menu-switch",
                icons::FOLDER,
                "Switch project",
                true,
                active == 0,
                theme,
            )
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                if *hovered {
                    this.open_project_menu_page(SpacesMenuPage::Projects, window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_project_menu_page(SpacesMenuPage::Projects, window, cx);
            })),
        );
        actions = actions.when(worktrees_enabled, |menu| {
            menu.child(
                self.project_menu_action_row(
                    "project-menu-worktree",
                    icons::GIT_BRANCH,
                    "Switch worktree",
                    true,
                    active == 1,
                    theme,
                )
                .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                    if *hovered {
                        this.open_project_menu_worktrees(window, cx);
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_project_menu_worktrees(window, cx);
                })),
            )
        });
        actions = actions.when(settings_enabled, |menu| {
            menu.child(
                self.project_menu_action_row(
                    "project-menu-settings",
                    icons::SETTINGS_MINIMALISTIC,
                    "Project settings",
                    true,
                    active == 1 + usize::from(worktrees_enabled),
                    theme,
                )
                .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                    if *hovered {
                        this.open_project_menu_page(SpacesMenuPage::Settings, window, cx);
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_project_menu_page(SpacesMenuPage::Settings, window, cx);
                })),
            )
        });
        let actions = crate::frost::frosted(popover::CARD_RADIUS, crate::frost::MENU_BLUR, actions)
            .into_any_element();

        let submenu: Option<AnyElement> = match page {
            SpacesMenuPage::Actions => None,
            SpacesMenuPage::Projects => {
                let rows = self.spaces_menu_rows(cx);
                let filter = self.settings.space_filter.clone();
                let now = Utc::now();
                let details: Vec<(SpacesMenuRow, SharedString, Option<SharedString>, bool)> = {
                    let state = self.state.read(cx);
                    rows.iter()
                        .map(|row| match row {
                            SpacesMenuRow::All => (row.clone(), "All projects".into(), None, false),
                            SpacesMenuRow::Space(id) => match state.space_row(id) {
                                Some(space) => {
                                    let (tag, offline) = state.space_device_tag(space, now);
                                    (
                                        row.clone(),
                                        space.display_name().to_string().into(),
                                        Some(tag.into()),
                                        offline,
                                    )
                                }
                                None => (row.clone(), "?".into(), None, false),
                            },
                            SpacesMenuRow::AddSpace => {
                                (row.clone(), "New project…".into(), None, false)
                            }
                        })
                        .collect()
                };
                let list = div()
                    .id("spaces-menu-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .track_scroll(&list_scroll)
                    .children(details.into_iter().enumerate().map(
                        |(ix, (row, label, tag, offline))| {
                            let is_selected = match &row {
                                SpacesMenuRow::All => filter.is_none(),
                                SpacesMenuRow::Space(id) => filter.as_deref() == Some(id.as_str()),
                                SpacesMenuRow::AddSpace => false,
                            };
                            let leading = if matches!(row, SpacesMenuRow::AddSpace) {
                                icons::PLUS
                            } else {
                                icons::FOLDER
                            };
                            let activate = row.clone();
                            popover::menu_row_nav(
                                theme,
                                is_selected,
                                ix == active,
                                format!("spaces-menu-row-{ix}"),
                                cx.entity_id(),
                            )
                            .id(("spaces-menu-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.activate_spaces_menu_row(activate.clone(), cx);
                            }))
                            .child(
                                icon(leading)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.8)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .when_some(tag, |el, tag| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(tag),
                                )
                                .when(offline, |el| {
                                    el.child(
                                        icon(icons::WIFI_OFF)
                                            .size(px(12.0))
                                            .flex_none()
                                            .text_color(theme.warning.opacity(0.8)),
                                    )
                                })
                            })
                        },
                    ));
                Some(
                    crate::frost::frosted(
                        popover::CARD_RADIUS,
                        crate::frost::MENU_BLUR,
                        popover::popover_card(theme)
                            .w(px(248.0))
                            .flex()
                            .flex_col()
                            .child(popover::search_input_frame(
                                theme,
                                search.clone().into_any_element(),
                            ))
                            .child(list)
                            .into_any_element(),
                    )
                    .into_any_element(),
                )
            }
            SpacesMenuPage::Settings => {
                let body = if let Some(space) = context_space {
                    let rename_id = space.id.clone();
                    let delete_id = space.id.clone();
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px(px(12.0))
                                .py(px(10.0))
                                .text_size(px(12.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(space.display_name().to_owned())),
                        )
                        .child(popover::menu_separator())
                        .child(
                            popover::menu_row(
                                theme,
                                false,
                                "project-settings-rename",
                                cx.entity_id(),
                            )
                            .id("project-settings-rename")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_spaces_menu(cx);
                                this.open_rename_space(rename_id.clone(), cx);
                            }))
                            .child(icon(icons::PEN).size(px(15.0)).text_color(theme.text_muted))
                            .child(SharedString::from("Rename…")),
                        )
                        .child(
                            popover::menu_row(
                                theme,
                                false,
                                "project-settings-delete",
                                cx.entity_id(),
                            )
                            .id("project-settings-delete")
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_spaces_menu(cx);
                                this.delete_space_confirm = Some(delete_id.clone());
                                this.space_delete_focus_pending = true;
                                cx.notify();
                            }))
                            .child(
                                icon(icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(15.0))
                                    .text_color(theme.danger),
                            )
                            .child(SharedString::from("Remove…")),
                        )
                } else {
                    div()
                        .p(px(12.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Select a project first")
                };
                Some(
                    crate::frost::frosted(
                        popover::CARD_RADIUS,
                        crate::frost::MENU_BLUR,
                        popover::popover_card(theme)
                            .w(px(220.0))
                            .flex()
                            .flex_col()
                            .child(body)
                            .into_any_element(),
                    )
                    .into_any_element(),
                )
            }
            SpacesMenuPage::Worktrees => {
                let Some(space) = context_space else {
                    return actions;
                };
                let pickers = self.composer.read(cx).pickers().clone();
                let refs = pickers.read(cx).worktree_menu_refs(&space.id);
                let query = search.read(cx).text().to_string();
                let rows = refs
                    .ready()
                    .map(|refs| {
                        let names: Vec<String> = refs.iter().map(|row| row.name.clone()).collect();
                        popover::filter_indices(&query, &names)
                            .into_iter()
                            .map(|ix| refs[ix].clone())
                            .collect::<Vec<RepoRef>>()
                    })
                    .unwrap_or_default();
                let selected = pickers.read(cx).draft().branch.clone();
                let body: AnyElement = match refs {
                    popover::Loadable::Idle | popover::Loadable::Loading => div()
                        .px(px(12.0))
                        .py(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading worktrees…")
                        .into_any_element(),
                    popover::Loadable::Error(message) => {
                        let retry_space = space.clone();
                        div()
                            .p(px(10.0))
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.danger)
                                    .child(SharedString::from(message)),
                            )
                            .child(
                                popover::menu_row(theme, false, "worktrees-retry", cx.entity_id())
                                    .id("worktrees-retry")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.composer.update(cx, |composer, cx| {
                                            composer.pickers().update(cx, |pickers, cx| {
                                                pickers
                                                    .retry_worktree_menu(retry_space.clone(), cx);
                                            });
                                        });
                                    }))
                                    .child(SharedString::from("Retry")),
                            )
                            .into_any_element()
                    }
                    popover::Loadable::Ready(_) if rows.is_empty() => div()
                        .px(px(12.0))
                        .py(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(if query.is_empty() {
                            "No worktrees found."
                        } else {
                            "No matching worktrees."
                        })
                        .into_any_element(),
                    popover::Loadable::Ready(_) => div()
                        .id("project-worktree-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().enumerate().map(|(ix, row)| {
                            let branch = row.name.clone();
                            let space_id = space.id.clone();
                            let is_selected = selected.as_deref() == Some(row.name.as_str());
                            popover::menu_row_nav(
                                theme,
                                is_selected,
                                ix == active,
                                format!("project-worktree-{ix}"),
                                cx.entity_id(),
                            )
                            .id(("project-worktree", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_project_worktree(space_id.clone(), branch.clone(), cx);
                            }))
                            .child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(row.name.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted.opacity(0.45))
                                    .child("worktree"),
                            )
                        }))
                        .into_any_element(),
                };
                Some(
                    crate::frost::frosted(
                        popover::CARD_RADIUS,
                        crate::frost::MENU_BLUR,
                        popover::popover_card(theme)
                            .w(px(280.0))
                            .flex()
                            .flex_col()
                            .child(popover::search_input_frame(
                                theme,
                                search.clone().into_any_element(),
                            ))
                            .child(body)
                            .into_any_element(),
                    )
                    .into_any_element(),
                )
            }
        };

        let mut menu = div()
            .id("spaces-filter-menu-surface")
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.spaces_menu_key(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.close_spaces_menu(cx);
            }))
            .flex()
            .flex_row()
            .items_start()
            .gap(px(2.0))
            .child(actions);
        if let Some(submenu) = submenu {
            menu = menu.child(submenu);
        }
        menu.into_any_element()
    }

    /// The sidebar's Sessions list: every session (idle included) of the
    /// filter space — or all spaces under "All" — attention-sorted. Rows are
    /// keyed for the FLIP resort glide.
    pub(crate) fn render_active_rows(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let filter = self.settings.space_filter.clone();
        let revision = self.sidebar_cache_revision(cx);
        let rows: Vec<super::SidebarSessionRow> = match &self.sidebar_active_cache {
            Some((cached_revision, cached_rows)) if *cached_revision == revision => {
                cached_rows.clone()
            }
            _ => {
                let state = self.state.read(cx);
                let rows: Vec<super::SidebarSessionRow> = state
                    .overview_chats(now)
                    .into_iter()
                    .filter(|(_, chat)| match &filter {
                        Some(space_id) => chat.space_id.as_deref() == Some(space_id.as_str()),
                        None => true,
                    })
                    .map(|(status, chat)| {
                        let space = state.space_for_chat(chat);
                        let mut folder = match (space, chat.space_id.as_deref()) {
                            (Some(space), _) => space.display_name().to_string(),
                            (None, None) => "~".to_string(),
                            (None, Some(_)) => "?".to_string(),
                        };
                        if let Some(device) = state.device_name(&chat.device_id) {
                            folder = format!("{folder} @ {device}");
                        }
                        let branch = chat
                            .branch
                            .as_deref()
                            .map(str::trim)
                            .filter(|b| !b.is_empty())
                            .map(str::to_string);
                        super::SidebarSessionRow {
                            status,
                            chat: chat.clone(),
                            folder,
                            branch,
                        }
                    })
                    .collect();
                self.sidebar_active_cache = Some((revision, rows.clone()));
                rows
            }
        };
        let selected = self.state.read(cx).selected_chat.clone();
        rows.into_iter()
            .map(|row| {
                let time_ago: SharedString =
                    format_time_ago(row.chat.last_message_at.unwrap_or(row.chat.created_at), now)
                        .into();
                let is_selected = selected.as_deref() == Some(row.chat.id.as_str());
                let height = super::CHAT_ROW_HEIGHT;
                let harness = row.chat.config.as_ref().map(|c| c.harness);
                let element = self.render_chat_row(
                    row.chat.id.clone(),
                    transcript::single_line(
                        &row.chat
                            .title
                            .clone()
                            .unwrap_or_else(|| "New session".into()),
                    )
                    .into(),
                    time_ago,
                    row.folder.into(),
                    row.branch.map(SharedString::from),
                    harness,
                    row.status,
                    is_selected,
                    false,
                    row.chat.pinned,
                    theme,
                    window,
                    cx,
                );
                (format!("c:{}", row.chat.id), height, element)
            })
            .collect()
    }

    /// Builds one collapsible folder header for the "All projects" grouped
    /// view: `[📁 icon] [Project name] ── [N] [▶/▼] [+]`.
    /// Clicking toggles `expanded_spaces` for the given space ID; the `+`
    /// re-homes the new-session canvas on that folder without leaving the
    /// grouped view (the project-less `~` group passes `None`, so it has no `+`).
    pub(crate) fn render_space_folder_header(
        &mut self,
        folder_id: Option<String>,
        name: SharedString,
        session_count: usize,
        is_collapsed: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The project-less "~" group has no folder (`None`): it collapses like
        // a folder but cannot host a new session or toggle in settings.
        let key = folder_id
            .clone()
            .unwrap_or_else(|| "__projectless__".to_string());
        let toggle_id = key.clone();
        let hover_key = format!("folder-header-{key}");
        let click_hover = hover_key.clone();
        let hover_text = hover_key.clone();
        // The + fades in with the header's own hover, so it is discoverable
        // without hunting for an invisible target; its wash follows its own
        // hover for the click affordance.
        let plus_key = format!("{hover_key}-plus");
        let plus_hover = plus_key.clone();
        let plus_space = folder_id.clone();
        div()
            .id(SharedString::from(format!("folder-header-{key}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .mt(px(4.0))
            .mb(px(2.0))
            .px(px(10.0))
            .h(px(28.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                &click_hover,
                crate::theme::wash(0.0),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener(hover_key, cx.entity_id()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.expanded_spaces.contains(&toggle_id) {
                    this.expanded_spaces.remove(&toggle_id);
                } else {
                    this.expanded_spaces.insert(toggle_id.clone());
                }
                this.settings.expanded_spaces = this.expanded_spaces.clone();
                this.schedule_save(cx);
                cx.notify();
            }))
            // Folder icon
            .child(
                crate::icons::icon(crate::icons::FOLDER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            // Project name (truncated)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(motion::hover_blend(
                        &hover_text,
                        theme.text.opacity(0.85),
                        theme.text,
                    ))
                    .child(name),
            )
            // Session count badge (shown when collapsed and has sessions)
            .when(is_collapsed && session_count > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.5))
                        .child(SharedString::from(format!("{session_count}"))),
                )
            })
            // Chevron
            .child(
                crate::icons::icon(if is_collapsed {
                    crate::icons::ALT_ARROW_RIGHT
                } else {
                    crate::icons::ALT_ARROW_DOWN
                })
                .size(px(12.0))
                .flex_none()
                .text_color(motion::hover_blend(
                    &hover_text,
                    theme.text_muted.opacity(0.5),
                    theme.text_muted.opacity(0.85),
                )),
            )
            // New session in this folder (real folders only; the `~` group has
            // no folder to start in). Kept extreme-right, always laid out so
            // hovering never shifts the rows; it fades in with header hover.
            .when(folder_id.is_some(), |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("folder-new-{key}")))
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .bg(motion::hover_blend(
                            &plus_hover,
                            crate::theme::wash(0.0),
                            crate::theme::wash(0.14),
                        ))
                        .on_hover(motion::hover_listener(plus_hover.clone(), cx.entity_id()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(space) = &plus_space {
                                this.open_new_session_in(Some(space.clone()), cx);
                            }
                        }))
                        .child(
                            crate::icons::icon(crate::icons::PLUS)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(motion::hover_blend(
                                    &hover_text,
                                    theme.text_muted.opacity(0.0),
                                    theme.text_muted.opacity(0.8),
                                )),
                        ),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)] // row render seam: call sites pass what they have
    pub(crate) fn render_grouped_session_row(
        &mut self,
        status: ChatIndicator,
        chat: komet_proto::Chat,
        folder: String,
        branch: Option<String>,
        selected: Option<&str>,
        now: chrono::DateTime<Utc>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_selected = selected == Some(chat.id.as_str());
        let time_ago: SharedString =
            format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
        let harness = chat.config.as_ref().map(|config| config.harness);
        self.render_chat_row(
            chat.id.clone(),
            transcript::single_line(&chat.title.clone().unwrap_or_else(|| "New session".into()))
                .into(),
            time_ago,
            folder.into(),
            branch.map(SharedString::from),
            harness,
            status,
            is_selected,
            false,
            chat.pinned,
            theme,
            window,
            cx,
        )
    }

    /// "All projects" sidebar view: sessions grouped by project folder.
    /// Each `Space` becomes a collapsible folder header; clicking toggles it.
    /// Projects with no sessions show a "No conversations yet" placeholder.
    /// Project-less sessions (space_id = None) are listed flat at the bottom
    /// under a "~" group, or omitted if none exist.
    pub(crate) fn render_grouped_rows(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let now = Utc::now();

        // Collect all needed data from state while holding read lock.
        type SessionEntry = (
            komet_proto::ChatIndicator,
            komet_proto::Chat,
            String,
            Option<String>,
        );
        struct SpaceGroup {
            id: String,
            name: String,
            sessions: Vec<SessionEntry>,
        }
        let (groups, projectless): (Vec<SpaceGroup>, Vec<SessionEntry>) = {
            let state = self.state.read(cx);
            let spaces = state.spaces_sorted();

            // Bucket chats by space in one pass (this runs on every Shell frame),
            // most recent first; only the rows that are rendered get cloned.
            let mut by_space: std::collections::HashMap<&str, Vec<&komet_proto::Chat>> =
                std::collections::HashMap::new();
            for chat in state.chats.iter().filter(|c| !c.archived) {
                if let Some(space_id) = chat.space_id.as_deref() {
                    by_space.entry(space_id).or_default().push(chat);
                }
            }
            for chats in by_space.values_mut() {
                chats.sort_by_key(|c| std::cmp::Reverse(c.last_message_at.unwrap_or(c.created_at)));
            }

            // Build groups: one per space in alphabetical order.
            let space_groups: Vec<SpaceGroup> = spaces
                .iter()
                .map(|space| {
                    let chats: Vec<komet_proto::Chat> = by_space
                        .get(space.id.as_str())
                        .map(|chats| chats.iter().map(|c| (*c).clone()).collect())
                        .unwrap_or_default();
                    let sessions = chats
                        .into_iter()
                        .map(|chat| {
                            let status = state.display_status_for(&chat, now);
                            let mut folder = space.display_name().to_string();
                            if let Some(device) = state.device_name(&chat.device_id) {
                                folder = format!("{folder} @ {device}");
                            }
                            let branch = chat
                                .branch
                                .as_deref()
                                .map(str::trim)
                                .filter(|b| !b.is_empty())
                                .map(str::to_string);
                            (status, chat, folder, branch)
                        })
                        .collect();
                    SpaceGroup {
                        id: space.id.clone(),
                        name: space.display_name().to_string(),
                        sessions,
                    }
                })
                .collect();

            // Project-less sessions (space_id = None, or dangling space).
            let mut orphan_chats: Vec<&komet_proto::Chat> = state
                .chats
                .iter()
                .filter(|c| !c.archived && c.space_id.is_none())
                .collect();
            orphan_chats
                .sort_by_key(|c| std::cmp::Reverse(c.last_message_at.unwrap_or(c.created_at)));
            let orphan_chats = orphan_chats.into_iter().cloned();
            let projectless_rows: Vec<(
                komet_proto::ChatIndicator,
                komet_proto::Chat,
                String,
                Option<String>,
            )> = orphan_chats
                .map(|chat| {
                    let status = state.display_status_for(&chat, now);
                    let mut folder = "~".to_string();
                    if let Some(device) = state.device_name(&chat.device_id) {
                        folder = format!("~ @ {device}");
                    }
                    let branch = chat
                        .branch
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                        .map(str::to_string);
                    (status, chat, folder, branch)
                })
                .collect();

            (space_groups, projectless_rows)
        };

        let selected = self.state.read(cx).selected_chat.clone();
        let mut elements: Vec<AnyElement> = Vec::new();

        if !self.settings.project_tree_expanded {
            let selected_session = groups
                .into_iter()
                .flat_map(|group| group.sessions)
                .chain(projectless)
                .find(|(_, chat, _, _)| selected.as_deref() == Some(chat.id.as_str()));
            if let Some((status, chat, folder, branch)) = selected_session {
                elements.push(self.render_grouped_session_row(
                    status,
                    chat,
                    folder,
                    branch,
                    selected.as_deref(),
                    now,
                    theme,
                    window,
                    cx,
                ));
            }
            return elements;
        }

        for group in groups {
            let is_collapsed = !self.expanded_spaces.contains(&group.id);
            let count = group.sessions.len();
            let header = self.render_space_folder_header(
                Some(group.id.clone()),
                SharedString::from(group.name),
                count,
                is_collapsed,
                theme,
                cx,
            );
            elements.push(header);

            if is_collapsed {
                if let Some((status, chat, folder, branch)) = group
                    .sessions
                    .into_iter()
                    .find(|(_, chat, _, _)| selected.as_deref() == Some(chat.id.as_str()))
                {
                    elements.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .pb(px(4.0))
                            .child(self.render_grouped_session_row(
                                status,
                                chat,
                                folder,
                                branch,
                                selected.as_deref(),
                                now,
                                theme,
                                window,
                                cx,
                            ))
                            .into_any_element(),
                    );
                }
            } else {
                if group.sessions.is_empty() {
                    // Placeholder: "No conversations yet"
                    elements.push(
                        div()
                            .px(px(Theme::SPACE_SM + 6.0))
                            .pb(px(4.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("No conversations yet"))
                            .into_any_element(),
                    );
                } else {
                    let mut rows_div = div().flex().flex_col().gap(px(2.0)).pb(px(4.0));
                    for (status, chat, folder, branch) in group.sessions {
                        let element = self.render_grouped_session_row(
                            status,
                            chat,
                            folder,
                            branch,
                            selected.as_deref(),
                            now,
                            theme,
                            window,
                            cx,
                        );
                        rows_div = rows_div.child(element);
                    }
                    elements.push(rows_div.into_any_element());
                }
            }
        }

        // Project-less sessions at the bottom (if any).
        if !projectless.is_empty() {
            let orphan_id = "__projectless__".to_string();
            let is_collapsed = !self.expanded_spaces.contains(&orphan_id);
            let count = projectless.len();
            let header = self.render_space_folder_header(
                None,
                SharedString::from("~"),
                count,
                is_collapsed,
                theme,
                cx,
            );
            elements.push(header);
            if is_collapsed {
                if let Some((status, chat, folder, branch)) = projectless
                    .into_iter()
                    .find(|(_, chat, _, _)| selected.as_deref() == Some(chat.id.as_str()))
                {
                    elements.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .pb(px(4.0))
                            .child(self.render_grouped_session_row(
                                status,
                                chat,
                                folder,
                                branch,
                                selected.as_deref(),
                                now,
                                theme,
                                window,
                                cx,
                            ))
                            .into_any_element(),
                    );
                }
            } else {
                let mut rows_div = div().flex().flex_col().gap(px(2.0)).pb(px(4.0));
                for (status, chat, folder, branch) in projectless {
                    let element = self.render_grouped_session_row(
                        status,
                        chat,
                        folder,
                        branch,
                        selected.as_deref(),
                        now,
                        theme,
                        window,
                        cx,
                    );
                    rows_div = rows_div.child(element);
                }
                elements.push(rows_div.into_any_element());
            }
        }

        elements
    }

    /// The sidebar's archived shelf — a direct port of t3code's settled
    /// shelf: header is label + hairline + chevron ("Archived (N)" closed,
    /// "Archived" open), rows are 36px SLIM one-liners (dimmed harness mark,
    /// title, time-ago right — the time yields to Unarchive on row hover),
    /// and the tail pages behind an explicit "Show N more" row (initial 10,
    /// +25 a click). `None` when nothing is archived under the current
    /// project filter.
    pub(crate) fn render_archived_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        const INITIAL: usize = 10;
        const PAGE: usize = 25;
        let now = Utc::now();
        let filter = self.settings.space_filter.clone();
        let rows: Vec<komet_proto::Chat> = {
            let state = self.state.read(cx);
            state
                .chats
                .iter()
                .filter(|c| c.archived)
                .filter(|chat| match &filter {
                    Some(space_id) => chat.space_id.as_deref() == Some(space_id.as_str()),
                    None => true,
                })
                .cloned()
                .collect()
        };
        if rows.is_empty() {
            return None;
        }
        let total = rows.len();
        let open = self.archived_open;
        let shown = self.archived_shown.max(INITIAL);
        // Header (t3code settled-shelf toggle): muted 12px label, a hairline
        // filling the middle, chevron flipping open/closed. The count only
        // shows while collapsed — expanded, the rows speak for themselves.
        let label: SharedString = if open {
            "Archived".into()
        } else {
            format!("Archived ({total})").into()
        };
        let header = div()
            .id("archived-toggle")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .mt(px(12.0))
            .mb(px(4.0))
            .px(px(10.0))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.archived_open = !this.archived_open;
                this.archived_shown = INITIAL;
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(label),
            )
            .child(div().h(px(1.0)).flex_1().bg(theme.border.opacity(0.6)))
            .child(
                crate::icons::icon(if open {
                    crate::icons::ALT_ARROW_DOWN
                } else {
                    crate::icons::ALT_ARROW_RIGHT
                })
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted.opacity(0.5)),
            );
        let mut section = div().flex().flex_col().child(header);
        if open {
            let selected = self.state.read(cx).selected_chat.clone();
            let selected_wash = crate::theme::glass_selected_bg();
            let mut list = div().flex().flex_col().gap(px(2.0));
            for chat in rows.into_iter().take(shown) {
                let id = chat.id.clone();
                let hovered = self.archived_hover.as_deref() == Some(id.as_str());
                let is_selected = selected.as_deref() == Some(id.as_str());
                let title: SharedString = transcript::single_line(
                    &chat.title.clone().unwrap_or_else(|| "New session".into()),
                )
                .into();
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let (mark, tint) = chat
                    .config
                    .as_ref()
                    .map(|c| crate::pickers::harness_brand_icon(c.harness))
                    .unwrap_or((crate::icons::CHAT_ROUND_LINE, None));
                // Right slot: time at rest; the Unarchive affordance takes
                // its place on row hover (t3code: "only the time/jump label
                // yields to the settle affordance").
                let right: AnyElement = if hovered {
                    let restore_id = id.clone();
                    // Metrics match the active rows' Archive pill exactly
                    // (18px pill, 11px icon, 10px label, padding bled right)
                    // — two sizes of the same affordance read as a mistake.
                    div()
                        .id(SharedString::from(format!("archived-restore-{id}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .h(px(18.0))
                        .px(px(4.0))
                        .mr(px(-4.0))
                        .rounded(px(5.0))
                        .bg(crate::theme::wash(0.10))
                        .hover(|s| s.bg(crate::theme::wash(0.18)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.set_chat_archived(restore_id.clone(), false, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::ARCHIVE_UP_MINIMALISTIC)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Unarchive")),
                        )
                        .into_any_element()
                } else {
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.55))
                        .child(time_ago)
                        .into_any_element()
                };
                let hover_id = id.clone();
                let open_id = id.clone();
                let menu_id = id.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("archived-{id}")))
                        .h(px(36.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .when(is_selected, |el| el.bg(selected_wash))
                        .when(!is_selected, |el| el.hover(|s| s.bg(theme.glass_hover())))
                        .on_hover(cx.listener(move |this, entered: &bool, _, cx| {
                            if *entered {
                                if this.archived_hover.as_deref() != Some(hover_id.as_str()) {
                                    this.archived_hover = Some(hover_id.clone());
                                    cx.notify();
                                }
                            } else if this.archived_hover.as_deref() == Some(hover_id.as_str()) {
                                this.archived_hover = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_chat(open_id.clone(), cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                this.chat_menu.open((menu_id.clone(), event.position));
                                cx.notify();
                            }),
                        )
                        // Archived history recedes: dimmed mark at rest,
                        // restored on hover (t3code's grayscale favicon).
                        .child(
                            crate::icons::icon(mark)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(if hovered || is_selected {
                                    tint.unwrap_or(theme.text_muted)
                                } else {
                                    tint.unwrap_or(theme.text_muted).opacity(0.4)
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(if hovered || is_selected {
                                    theme.text
                                } else {
                                    theme.text.opacity(0.55)
                                })
                                .child(title),
                        )
                        .child(right),
                );
            }
            // Mount fade: the shelf popping in whole read as jank — a quick
            // fade on the expanded list softens the accordion.
            section = section.child(motion::fade_quick("archived-list", list));
            if total > shown {
                let remaining = (total - shown).min(PAGE);
                section = section.child(
                    div()
                        .id("archived-more")
                        // Sits outside the rows' gapped column — match the
                        // list's 2px row gap or it fuses with the last row.
                        .mt(px(2.0))
                        .h(px(36.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted.opacity(0.55))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archived_shown = this.archived_shown.max(INITIAL) + PAGE;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::PLUS)
                                .size(px(14.0))
                                .flex_none(),
                        )
                        .child(SharedString::from(format!("Show {remaining} more"))),
                );
            }
        }
        Some(section.pb(px(Theme::SPACE_SM)).into_any_element())
    }
}
