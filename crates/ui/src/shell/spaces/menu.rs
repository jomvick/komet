//! Space filter dropdown: the space/projet picker rows, its actions and its keys.

use super::*;

impl Shell {
    // ---- space filter ----

    /// Set the sidebar's session filter (`None` = All spaces). On the
    /// new-session canvas the space context follows the filter — the canvas
    /// default is "the space you're looking at".
    pub(crate) fn set_space_filter(&mut self, filter: Option<String>, cx: &mut Context<Self>) {
        self.settings.space_filter = filter.clone();
        if let Some(space_id) = filter
            && self.state.read(cx).selected_chat.is_none()
        {
            self.state
                .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        }
        self.close_spaces_menu(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Close the space-filter dropdown through the exit animation (no-op when
    /// it isn't open). Every close path funnels here so the menu always
    /// animates out instead of vanishing.
    pub(crate) fn close_spaces_menu(&mut self, cx: &mut Context<Self>) {
        if self.spaces_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.spaces_menu);
            cx.notify();
        }
    }

    /// Land in a just-added space: filter the sidebar to it and open the
    /// new-session canvas there.
    pub(crate) fn land_in_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.settings.space_filter = Some(space_id.clone());
        self.settings.last_space_id = Some(space_id.clone());
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id), cx);
            s.select_chat(None, cx);
        });
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- sidebar sections ----

    /// The filter's display rows: "All projects", then spaces matching the
    /// search (ranked — `popover::filter_indices`), then "New project…".
    /// "All" only shows on an empty query (searching means hunting a space).
    pub(crate) fn spaces_menu_rows(&self, cx: &App) -> Vec<SpacesMenuRow> {
        let query = self
            .spaces_menu
            .get()
            .map(|menu| menu.search.read(cx).text().to_string())
            .unwrap_or_default();
        let state = self.state.read(cx);
        let spaces = state.spaces_sorted();
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        let mut rows: Vec<SpacesMenuRow> = Vec::new();
        if query.trim().is_empty() {
            rows.push(SpacesMenuRow::All);
        }
        rows.extend(
            popover::filter_indices(&query, &names)
                .into_iter()
                .map(|ix| SpacesMenuRow::Space(spaces[ix].id.clone())),
        );
        rows.push(SpacesMenuRow::AddSpace);
        rows
    }

    pub(crate) fn open_spaces_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // "PaletteSearch" context: ↑↓/⏎ stay unbound in the input and bubble
        // to the card's key handler.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search projects…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(menu) = this.spaces_menu.open_mut() {
                    menu.active = 0;
                }
                cx.notify();
            }
        });
        let focus = cx.focus_handle();
        self.spaces_menu.open(SpacesMenu {
            search,
            page: SpacesMenuPage::Actions,
            active: 0,
            focus: focus.clone(),
            list_scroll: gpui::ScrollHandle::new(),
            _search_events: search_events,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn open_project_menu_page(
        &mut self,
        page: SpacesMenuPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        if let Some(menu) = self.spaces_menu.open_mut() {
            changed = menu.page != page;
            menu.page = page;
            menu.active = 0;
            menu.list_scroll.set_offset(gpui::Point::default());
            if changed {
                let placeholder = match page {
                    SpacesMenuPage::Projects => "Search projects…",
                    SpacesMenuPage::Worktrees => "Search worktrees…",
                    _ => "Search…",
                };
                menu.search.update(cx, |search, cx| {
                    search.set_text("", cx);
                    search.set_placeholder(placeholder, cx);
                });
            }
            if matches!(page, SpacesMenuPage::Projects | SpacesMenuPage::Worktrees) {
                let handle = menu.search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            } else {
                let handle = menu.focus.clone();
                window.focus(&handle, cx);
            }
        }
        if changed
            && page == SpacesMenuPage::Worktrees
            && let Some(space) = self.context_space(cx)
        {
            self.composer.update(cx, |composer, cx| {
                composer
                    .pickers()
                    .update(cx, |pickers, cx| pickers.prepare_worktree_menu(space, cx));
            });
        }
        cx.notify();
    }

    pub(crate) fn activate_spaces_menu_row(&mut self, row: SpacesMenuRow, cx: &mut Context<Self>) {
        match row {
            SpacesMenuRow::All => self.set_space_filter(None, cx),
            SpacesMenuRow::Space(id) => self.set_space_filter(Some(id), cx),
            SpacesMenuRow::AddSpace => {
                self.close_spaces_menu(cx);
                self.open_add_space(cx);
            }
        }
    }

    pub(crate) fn context_space(&self, cx: &App) -> Option<Space> {
        let state = self.state.read(cx);
        let id = self
            .settings
            .space_filter
            .clone()
            .or_else(|| {
                state
                    .selected_chat_row()
                    .and_then(|chat| chat.space_id.clone())
            })
            .or_else(|| state.selected_space.clone())?;
        state.space_row(&id).cloned()
    }

    pub(crate) fn project_menu_action_availability(&self, cx: &App) -> (bool, bool) {
        let has_project_filter = self.settings.space_filter.is_some();
        let context_space = self.context_space(cx);
        let worktrees_enabled = has_project_filter
            && context_space
                .as_ref()
                .is_some_and(|space| space.git_detected);
        let settings_enabled = has_project_filter && context_space.is_some();
        (worktrees_enabled, settings_enabled)
    }

    pub(crate) fn open_project_menu_worktrees(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_project_menu_page(SpacesMenuPage::Worktrees, window, cx);
    }

    pub(crate) fn select_project_worktree(
        &mut self,
        space_id: String,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        self.open_new_session_in(Some(space_id), cx);
        let composer = self.composer.clone();
        cx.defer(move |cx| {
            composer.update(cx, |composer, cx| {
                composer.pickers().update(cx, |pickers, cx| {
                    pickers.select_worktree_from_menu(&branch, cx);
                });
            });
        });
        self.close_spaces_menu(cx);
    }

    pub(crate) fn project_menu_action_row(
        &self,
        id: &'static str,
        icon_path: &'static str,
        label: &'static str,
        enabled: bool,
        active: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(32.0))
            .mx(px(4.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.5))
            .text_color(if enabled {
                theme.text
            } else {
                theme.text_faint.opacity(0.45)
            })
            .when(enabled, |el| {
                el.cursor_pointer().hover(|el| el.bg(theme.element_hover))
            })
            .when(active, |el| el.bg(theme.element_active))
            .child(icon(icon_path).size(px(15.0)).text_color(theme.text_muted))
            .child(div().flex_1().child(SharedString::from(label)))
            .child(
                icon(icons::ALT_ARROW_RIGHT)
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.55)),
            )
    }

    /// Dropdown keys (bubbling from the focused search input): ↑↓ navigate,
    /// ⏎ activates the highlighted row, esc closes.
    pub(crate) fn spaces_menu_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The card stays mounted (and focused) through the exit animation —
        // keys must not drive a dying menu.
        if !self.spaces_menu.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.close_spaces_menu(cx);
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = match self.spaces_menu.get().map(|menu| menu.page) {
                    Some(SpacesMenuPage::Actions) => {
                        let (worktrees_enabled, settings_enabled) =
                            self.project_menu_action_availability(cx);
                        1 + usize::from(worktrees_enabled) + usize::from(settings_enabled)
                    }
                    Some(SpacesMenuPage::Settings) => 2,
                    Some(SpacesMenuPage::Worktrees) => self
                        .context_space(cx)
                        .map(|space| {
                            let pickers = self.composer.read(cx).pickers().clone();
                            pickers
                                .read(cx)
                                .worktree_menu_refs(&space.id)
                                .ready()
                                .map_or(0, |refs| {
                                    let query = self
                                        .spaces_menu
                                        .get()
                                        .map(|menu| menu.search.read(cx).text().to_string())
                                        .unwrap_or_default();
                                    let names: Vec<String> =
                                        refs.iter().map(|row| row.name.clone()).collect();
                                    popover::filter_indices(&query, &names).len()
                                })
                        })
                        .unwrap_or(0),
                    _ => self.spaces_menu_rows(cx).len(),
                };
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.spaces_menu.open_mut() {
                    menu.active = popover::menu_step(Some(menu.active), count, delta).unwrap_or(0);
                    menu.list_scroll.scroll_to_item(menu.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let (worktrees_enabled, settings_enabled) =
                    self.project_menu_action_availability(cx);
                match self.spaces_menu.get().map(|menu| (menu.page, menu.active)) {
                    Some((SpacesMenuPage::Actions, 0)) => {
                        self.open_project_menu_page(SpacesMenuPage::Projects, window, cx);
                        return;
                    }
                    Some((SpacesMenuPage::Actions, 1)) if worktrees_enabled => {
                        self.open_project_menu_worktrees(window, cx);
                        return;
                    }
                    Some((SpacesMenuPage::Actions, active))
                        if settings_enabled && active == 1 + usize::from(worktrees_enabled) =>
                    {
                        self.open_project_menu_page(SpacesMenuPage::Settings, window, cx);
                        return;
                    }
                    Some((SpacesMenuPage::Settings, 0)) => {
                        if let Some(space) = self.context_space(cx) {
                            self.close_spaces_menu(cx);
                            self.open_rename_space(space.id, cx);
                        }
                        return;
                    }
                    Some((SpacesMenuPage::Settings, 1)) => {
                        if let Some(space) = self.context_space(cx) {
                            self.close_spaces_menu(cx);
                            self.delete_space_confirm = Some(space.id);
                            self.space_delete_focus_pending = true;
                            cx.notify();
                        }
                        return;
                    }
                    Some((SpacesMenuPage::Worktrees, active)) => {
                        if let Some(space) = self.context_space(cx) {
                            let pickers = self.composer.read(cx).pickers().clone();
                            let rows = pickers
                                .read(cx)
                                .worktree_menu_refs(&space.id)
                                .ready()
                                .cloned()
                                .unwrap_or_default();
                            let query = self
                                .spaces_menu
                                .get()
                                .map(|menu| menu.search.read(cx).text().to_string())
                                .unwrap_or_default();
                            let names: Vec<String> =
                                rows.iter().map(|row| row.name.clone()).collect();
                            if let Some(row) = popover::filter_indices(&query, &names)
                                .into_iter()
                                .nth(active)
                            {
                                let branch = rows[row].name.clone();
                                self.select_project_worktree(space.id, branch, cx);
                            }
                        }
                        return;
                    }
                    _ => {}
                }
                // Only the Projects page renders `spaces_menu_rows`; the other
                // pages handle their rows above, so an unmatched Enter is a no-op.
                if self.spaces_menu.get().map(|m| m.page) == Some(SpacesMenuPage::Projects) {
                    let row = {
                        let active = self.spaces_menu.get().map(|m| m.active).unwrap_or(0);
                        self.spaces_menu_rows(cx).get(active).cloned()
                    };
                    if let Some(row) = row {
                        self.activate_spaces_menu_row(row, cx);
                    }
                }
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }
}
