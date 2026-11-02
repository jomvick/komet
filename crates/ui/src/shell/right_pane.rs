//! Right-pane surfaces: tabs, files/diff/subagent/terminal openers and close flow.

use super::*;

impl Shell {
    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. Also available on the
    /// new-session canvas now (user request, reversing an earlier one) —
    /// `panel_key` already keys canvas state under `space-canvas:{id}`, so
    /// per-panel storage (tabs, changes_open, right_active) works
    /// identically whether or not a chat is active.
    pub(super) fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open
    }

    /// The current chat's terminal flag (per-session, in-memory).
    pub(super) fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    pub(super) fn right_target(&self, cx: &App) -> f32 {
        if !self.right_pane_open(cx) {
            0.0
        } else if self.right_pane_expanded {
            // Takeover: everything right of the sidebar; the conversation
            // column (flex_1) collapses to zero behind it. Rides the sidebar's
            // width tween so a sidebar toggle mid-takeover stays seamless.
            let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            (self.viewport_width - sidebar_now).max(RIGHT_PANE_MIN)
        } else {
            // The Files+FileEditor split (render_right_pane) spends a fixed
            // 220px on the tree column; below SPLIT_RIGHT_PANE_MIN total, the
            // remaining content column is too narrow to read (user report:
            // truncated/wrapped-to-nothing file preview). Only the split
            // needs the wider floor — Diff/Terminal/a lone FileEditor stay at
            // the normal minimum, and a user's wider manual resize always wins
            // via `.max`.
            let min = if self.files_split_active(cx) {
                SPLIT_RIGHT_PANE_MIN
            } else {
                RIGHT_PANE_MIN
            };
            self.settings.right_pane_width.max(min)
        }
    }

    /// Whether `render_right_pane` will draw the tree+content split for the
    /// panel key ~as `right_target` sees it: a Files surface tab exists
    /// alongside whichever surface is active, and that active surface isn't
    /// the tree itself (matches the `files_panel` condition in
    /// `render_right_pane` by construction, not a duplicated match).
    pub(super) fn files_split_active(&self, cx: &App) -> bool {
        !matches!(self.resolved_right_active(cx), RightSurface::Files)
            && self
                .right_surface_rows(cx)
                .iter()
                .any(|(s, _, _, _)| matches!(s, RightSurface::Files))
    }

    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git gate: the pane hosts terminals too (see `right_pane_open`).
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if !open {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }

    pub(super) fn right_terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.right_terminal {
            return terminal.clone();
        }
        let prefs = self.terminal_prefs();
        let terminal = cx.new(|cx| TerminalPanel::new_embedded(self.state.clone(), cx));
        terminal.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        let sub = cx.subscribe(&terminal, |this, panel, event: &TerminalPanelEvent, cx| {
            this.on_terminal_panel_event(panel, event, cx);
        });
        self.right_terminal_event_sub = Some(sub);
        self.right_terminal = Some(terminal.clone());
        terminal
    }

    /// The right pane's surface tabs in the STORED (drag-reorderable) order —
    /// `(surface, title, dirty, tooltip_detail)`; entries whose backing tab/entity
    /// is gone are skipped.
    pub(super) fn right_surface_rows(
        &self,
        cx: &App,
    ) -> Vec<(RightSurface, SharedString, bool, Option<SharedString>)> {
        let key = self.panel_key(cx);
        let stored: &[RightSurface] = self
            .right_tabs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let terminals: Vec<(u64, SharedString, bool)> = self
            .right_terminal
            .as_ref()
            .map(|t| t.read(cx).tab_summaries(cx))
            .unwrap_or_default();
        stored
            .iter()
            .filter_map(|surface| match surface {
                RightSurface::Files => self.files.get(&key).map(|files| {
                    let files = files.read(cx);
                    (
                        *surface,
                        files.tab_title(),
                        files.has_unsaved_changes(),
                        None,
                    )
                }),
                RightSurface::File(id) => self.file_surfaces.get(id).map(|file| {
                    let path = self.file_surface_paths.get(id);
                    let title = path
                        .map(|p| workspace_file_title(p))
                        .unwrap_or_else(|| SharedString::from("File"));
                    (
                        *surface,
                        title,
                        file.read(cx).has_unsaved_changes(),
                        path.cloned().map(Into::into),
                    )
                }),
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    // Contextual title (user request): the pane's scope
                    // label, or the pinned commit's subject.
                    .map(|changes| (*surface, changes.read(cx).tab_title(), false, None)),
                RightSurface::Terminal(tab) => terminals
                    .iter()
                    .find(|(k, _, _)| k == tab)
                    .map(|(_, title, _)| (*surface, title.clone(), false, None)),
                RightSurface::Picker => None,
                RightSurface::Subagent(id) => self
                    .subagent_tabs
                    .get(id)
                    .map(|panel| (*surface, panel.read(cx).tab_title(), false, None)),
                RightSurface::AgentShell => Some((
                    *surface,
                    SharedString::from("Shell agent"),
                    false,
                    Some(SharedString::from(
                        "Read-only view of the agent's own shell",
                    )),
                )),
            })
            .collect()
    }

    /// Drag-reorder a surface tab within this chat's strip.
    pub(super) fn reorder_right_tabs(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key)
            && from < tabs.len()
            && to < tabs.len()
            && from != to
        {
            let surface = tabs.remove(from);
            tabs.insert(to, surface);
            cx.notify();
        }
    }

    /// Track the hovered drop slot mid-drag (the terminal drawer's
    /// `update_drag_over`, ported: epoch bumps restart the slide tween).
    pub(super) fn update_right_tab_drag_over(
        &mut self,
        from: usize,
        over: usize,
        cx: &mut Context<Self>,
    ) {
        match &mut self.right_tab_drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.right_tab_drag = Some(RightTabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// The surface that actually renders: the stored pick when it still
    /// exists, else the first remaining tab, else the picker. Terminal keys
    /// go stale when their tab closes/exits — never render a dead surface.
    pub(super) fn resolved_right_active(&self, cx: &App) -> RightSurface {
        let picked = self.panels.get(&self.panel_key(cx)).right_active;
        let rows = self.right_surface_rows(cx);
        let exists = match picked {
            RightSurface::Picker => false,
            surface => rows.iter().any(|(s, _, _, _)| *s == surface),
        };
        if exists {
            picked
        } else {
            rows.first()
                .map(|(s, _, _, _)| *s)
                .unwrap_or(RightSurface::Picker)
        }
    }

    pub(super) fn set_right_active(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        self.panels.update(&key, |p| p.right_active = surface);
        match surface {
            RightSurface::Files => {
                if let Some(files) = self.files.get(&key).cloned() {
                    files.update(cx, |files, cx| files.ensure_loaded(cx));
                }
            }
            RightSurface::File(id) => {
                if let Some(file) = self.file_surfaces.get(&id).cloned() {
                    file.update(cx, |file, cx| file.ensure_loaded(cx));
                }
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
            }
            RightSurface::Diff(id) => {
                if let Some(changes) = self.diffs.get(&id).cloned() {
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                }
            }
            RightSurface::Subagent(id) => {
                if let Some(panel) = self.subagent_tabs.get(&id).cloned() {
                    panel.update(cx, |panel, cx| panel.ensure_started(cx));
                }
            }
            RightSurface::AgentShell => {
                // Nothing to load or start: the surface reads the transcript
                // at paint time and owns no subscription by design.
            }
            RightSurface::Picker => {}
        }
        cx.notify();
    }

    pub(super) fn focus_right_file_editor(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        let files = match surface {
            RightSurface::Files => self.files.get(&key).cloned(),
            RightSurface::File(id) => self.file_surfaces.get(&id).cloned(),
            _ => None,
        };
        if let Some(files) = files {
            files.update(cx, |files, cx| {
                files.focus_editor(window, cx);
            });
        }
    }

    /// Files is single-instance per chat: both the picker and the `+` menu
    /// focus the existing surface instead of creating duplicate trees and
    /// duplicate workspace subscriptions.
    pub(super) fn add_files_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if !self.files.contains_key(&key) {
            let delay = self.settings.files_autosave_delay_ms;
            let editor_font_size = self.settings.files_editor_font_size;
            let word_wrap = self.settings.files_word_wrap;
            let show_all_files = self.settings.files_show_all;
            let files = cx.new(|cx| {
                FilesSurface::new(
                    self.state.clone(),
                    key.clone(),
                    delay,
                    editor_font_size,
                    word_wrap,
                    show_all_files,
                    cx,
                )
            });
            let event_key = key.clone();
            let sub = cx.subscribe_in(
                &files,
                window,
                move |this: &mut Self, _, event, window, cx| match event {
                    FilesEvent::OpenFile(path) => {
                        this.add_file_surface(path.clone(), None, window, cx)
                    }
                    FilesEvent::TitleChanged => cx.notify(),
                    FilesEvent::FileRenamed { .. } => cx.notify(),
                    FilesEvent::WordWrapChanged(word_wrap) => {
                        this.set_files_word_wrap(*word_wrap, window, cx)
                    }
                    FilesEvent::ShowAllFilesChanged(show_all_files) => {
                        this.set_files_show_all(*show_all_files, cx)
                    }
                    FilesEvent::CloseReady => {
                        this.on_file_close_ready(RightSurface::Files, &event_key, cx)
                    }
                    FilesEvent::CloseCancelled => this.cancel_file_close(RightSurface::Files, cx),
                },
            );
            self.files.insert(key.clone(), files);
            self.files_subs.insert(key.clone(), sub);
        }
        let tabs = self.right_tabs.entry(key).or_default();
        push_unique_right_surface(tabs, RightSurface::Files);
        self.set_right_active(RightSurface::Files, cx);
        self.focus_right_file_editor(RightSurface::Files, window, cx);
    }

    /// Open a workspace file as a first-class right-pane tab. Every editor is
    /// a separate FilesSurface so its tree, search, watcher and split layout
    /// stay stable while users move among open files.
    pub(super) fn add_file_surface(
        &mut self,
        path: String,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_key = self.panel_key(cx);
        let lookup = (panel_key.clone(), path.clone());
        if let Some(id) = self.file_surface_keys.get(&lookup).copied() {
            let surface = RightSurface::File(id);
            self.set_right_active(surface, cx);
            if let Some(line) = line
                && let Some(file) = self.file_surfaces.get(&id)
            {
                file.update(cx, |file, cx| file.open_at_line(line, cx));
            }
            self.focus_right_file_editor(surface, window, cx);
            return;
        }

        self.file_surface_seq += 1;
        let id = self.file_surface_seq;
        let file = cx.new(|cx| {
            FilesSurface::new_editor(
                self.state.clone(),
                panel_key.clone(),
                path.clone(),
                self.settings.files_autosave_delay_ms,
                self.settings.files_editor_font_size,
                self.settings.files_word_wrap,
                self.settings.files_show_all,
                cx,
            )
        });
        let event_panel_key = panel_key.clone();
        let sub = cx.subscribe_in(
            &file,
            window,
            move |this: &mut Self, _, event, window, cx| match event {
                FilesEvent::OpenFile(path) => this.add_file_surface(path.clone(), None, window, cx),
                FilesEvent::TitleChanged => cx.notify(),
                FilesEvent::FileRenamed { old_path, new_path } => {
                    this.rename_file_surface(id, &event_panel_key, old_path, new_path, cx)
                }
                FilesEvent::WordWrapChanged(word_wrap) => {
                    this.set_files_word_wrap(*word_wrap, window, cx)
                }
                FilesEvent::ShowAllFilesChanged(show_all_files) => {
                    this.set_files_show_all(*show_all_files, cx)
                }
                FilesEvent::CloseReady => {
                    this.on_file_close_ready(RightSurface::File(id), &event_panel_key, cx)
                }
                FilesEvent::CloseCancelled => this.cancel_file_close(RightSurface::File(id), cx),
            },
        );
        if let Some(line) = line {
            file.update(cx, |file, cx| file.open_at_line(line, cx));
        }
        self.file_surfaces.insert(id, file);
        self.file_surface_paths.insert(id, path);
        self.file_surface_keys.insert(lookup, id);
        self.file_surface_subs.insert(id, sub);
        self.right_tabs
            .entry(panel_key)
            .or_default()
            .push(RightSurface::File(id));
        self.set_right_active(RightSurface::File(id), cx);
    }

    pub(super) fn rename_file_surface(
        &mut self,
        id: u64,
        panel_key: &str,
        old_path: &str,
        new_path: &str,
        cx: &mut Context<Self>,
    ) {
        if self.file_surface_paths.get(&id).map(String::as_str) != Some(old_path) {
            return;
        }
        self.file_surface_paths.insert(id, new_path.to_string());
        self.file_surface_keys
            .remove(&(panel_key.to_string(), old_path.to_string()));
        self.file_surface_keys
            .entry((panel_key.to_string(), new_path.to_string()))
            .or_insert(id);
        cx.notify();
    }

    pub(super) fn workspace_path_for_surface(
        &self,
        surface: RightSurface,
        cx: &App,
    ) -> Option<WorkspacePathDrag> {
        let path = match surface {
            RightSurface::Files => self
                .files
                .get(&self.panel_key(cx))
                .and_then(|files| files.read(cx).attachment_path().map(str::to_string))?,
            RightSurface::File(id) => self.file_surface_paths.get(&id).cloned()?,
            RightSurface::Diff(_)
            | RightSurface::Picker
            | RightSurface::Terminal(_)
            | RightSurface::Subagent(_)
            | RightSurface::AgentShell => {
                return None;
            }
        };
        Some(WorkspacePathDrag::new(path, false))
    }

    pub(super) fn set_files_word_wrap(
        &mut self,
        word_wrap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.files_word_wrap = word_wrap;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_word_wrap(word_wrap, cx));
        }
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_word_wrap(word_wrap, window, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn set_files_editor_font_size(
        &mut self,
        editor_font_size: f32,
        cx: &mut Context<Self>,
    ) {
        self.settings.files_editor_font_size = editor_font_size;
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_editor_font_size(editor_font_size, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn set_files_show_all(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        self.settings.files_show_all = show_all_files;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_show_all_files(show_all_files, cx));
        }
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_show_all_files(show_all_files, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn on_file_close_ready(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pending_file_closes.contains(&surface) {
            self.complete_file_close(surface, panel_key, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn cancel_file_close(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        self.pending_file_closes.remove(&surface);
        self.window_close_pending = false;
        cx.notify();
    }

    pub(super) fn complete_file_close(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(tabs) = self.right_tabs.get_mut(panel_key) {
            tabs.retain(|candidate| *candidate != surface);
        }
        match surface {
            RightSurface::Files => {
                self.files.remove(panel_key);
                self.files_subs.remove(panel_key);
            }
            RightSurface::File(id) => {
                self.file_surfaces.remove(&id);
                self.file_surface_paths.remove(&id);
                self.file_surface_subs.remove(&id);
                self.file_surface_keys.retain(|_, value| *value != id);
            }
            _ => return,
        }
        self.pending_file_closes.remove(&surface);
        self.panels.update(panel_key, |panel| {
            if panel.right_active == surface {
                panel.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    /// A surface tab's ✕. For Files/File surfaces, initiates a graceful
    /// close that waits for pending autosaves to flush.
    pub(super) fn close_right_surface(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        let files = match surface {
            RightSurface::Files => self.files.get(&key).cloned(),
            RightSurface::File(id) => self.file_surfaces.get(&id).cloned(),
            _ => None,
        };
        if let Some(files) = files {
            match files.update(cx, |files, cx| files.prepare_close(cx)) {
                FilesCloseDisposition::Allow => {
                    self.complete_file_close(surface, &key, cx);
                }
                FilesCloseDisposition::Pending | FilesCloseDisposition::Blocked => {
                    self.pending_file_closes.insert(surface);
                    self.set_right_active(surface, cx);
                }
            }
            return;
        }
        if let Some(tabs) = self.right_tabs.get_mut(&key) {
            tabs.retain(|s| *s != surface);
        }
        match surface {
            RightSurface::Files | RightSurface::File(_) => {}
            RightSurface::Diff(id) => {
                // Dropping the entity tears down its diff watch.
                self.diffs.remove(&id);
                self.diff_subs.remove(&id);
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.close_tab_by_key(tab, window, cx));
            }
            RightSurface::Subagent(id) => {
                // Dropping the entity ends its subscription tasks: a tab that
                // is closed is a feed nobody is watching.
                self.subagent_tabs.remove(&id);
                self.subagent_subs.remove(&id);
            }
            RightSurface::AgentShell => {
                // Nothing to tear down: the read-only surface owns no entity,
                // no subscription, and no PTY.
            }
            RightSurface::Picker => {}
        }
        self.panels.update(&key, |p| {
            if p.right_active == surface {
                p.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    pub fn prepare_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        if surfaces
            .iter()
            .all(|surface| !surface.read(cx).has_unsaved_changes())
        {
            self.window_close_pending = false;
            return true;
        }
        self.window_close_pending = true;
        let mut all_ready = true;
        for surface in surfaces {
            let disposition = surface.update(cx, |surface, cx| surface.prepare_close(cx));
            all_ready &= disposition == FilesCloseDisposition::Allow;
        }
        if all_ready {
            self.window_close_pending = false;
        }
        all_ready
    }

    pub(super) fn all_file_edits_flushed(&self, cx: &App) -> bool {
        self.files
            .values()
            .chain(self.file_surfaces.values())
            .all(|surface| !surface.read(cx).has_unsaved_changes())
    }
    pub(super) fn add_diff_surface(&mut self, cx: &mut Context<Self>) {
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.register_diff_surface(changes, cx);
    }

    /// A History row click: the commit opens as its own pinned diff tab
    /// (user request).
    pub(super) fn add_commit_diff_surface(
        &mut self,
        commit: komet_proto::GitHistoryCommit,
        cx: &mut Context<Self>,
    ) {
        let changes = cx.new(|cx| Changes::for_commit(self.state.clone(), commit, cx));
        self.register_diff_surface(changes, cx);
    }

    pub(super) fn register_diff_surface(
        &mut self,
        changes: Entity<Changes>,
        cx: &mut Context<Self>,
    ) {
        self.diff_seq += 1;
        let id = self.diff_seq;
        let sub = cx.subscribe(&changes, |this: &mut Self, _, event, cx| match event {
            ChangesEvent::OpenCommit(commit) => {
                this.add_commit_diff_surface(commit.clone(), cx);
            }
        });
        self.diffs.insert(id, changes);
        self.diff_subs.insert(id, sub);
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Diff(id));
        self.set_right_active(RightSurface::Diff(id), cx);
    }

    /// A spawn chip's badge click: the child's transcript opens as its own
    /// read-only tab, over the host's run journal (`WatchSubagent`).
    pub(super) fn add_subagent_surface(&mut self, open: SubagentOpen, cx: &mut Context<Self>) {
        // One tab per feed: clicking the same chip again — or a chip whose
        // child was already opened — focuses the tab it opened rather than
        // starting a second subscription to the same journal feed.
        let id = match self
            .subagent_tabs
            .iter()
            .find(|(_, panel)| {
                // Feed identity only: status/counters/title change while the
                // tab stays open, and must not spawn a second subscription.
                let existing = panel.read(cx).open();
                existing.chat_id == open.chat_id
                    && existing.target_device_id == open.target_device_id
                    && existing.spawn.parent_tool_use_id == open.spawn.parent_tool_use_id
            })
            .map(|(id, _)| *id)
        {
            Some(id) => id,
            None => {
                self.subagent_seq += 1;
                let id = self.subagent_seq;
                let panel = cx.new(|_| SubagentPanel::new(self.state.clone(), open));
                let sub = cx.subscribe(&panel, |this: &mut Self, _, event, cx| match event {
                    SubagentEvent::OpenNested(nested) => {
                        this.add_subagent_surface((**nested).clone(), cx);
                    }
                });
                self.subagent_tabs.insert(id, panel);
                self.subagent_subs.insert(id, sub);
                id
            }
        };
        let surface = RightSurface::Subagent(id);
        let key = self.panel_key(cx);
        push_unique_right_surface(self.right_tabs.entry(key.clone()).or_default(), surface);
        // A chip is clicked from the conversation, where the pane may well be
        // closed — opening a tab the user cannot see would read as the click
        // doing nothing.
        if !self.right_pane_open(cx) {
            let from = self.right_target(cx);
            self.panels.update(&key, |p| p.changes_open = true);
            self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        }
        self.set_right_active(surface, cx);
    }

    /// The picker's « Shell agent » card: the read-only log of the commands
    /// the agent ran in its OWN shell (Claude/Codex keep that traffic out of
    /// any Komet PTY). One tab per chat — it projects the transcript's tool
    /// events, so a second tab would show the identical list.
    pub(super) fn add_agent_shell_surface(&mut self, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        push_unique_right_surface(
            self.right_tabs.entry(key.clone()).or_default(),
            RightSurface::AgentShell,
        );
        // Same rule as a spawn chip's tab: opening a surface the user cannot
        // see reads as the click doing nothing.
        if !self.right_pane_open(cx) {
            let from = self.right_target(cx);
            self.panels.update(&key, |p| p.changes_open = true);
            self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        }
        self.set_right_active(RightSurface::AgentShell, cx);
    }

    /// The picker's Terminal card / the `+` menu's Terminal row: every click
    /// opens a fresh embedded terminal tab.
    pub(super) fn add_terminal_surface(&mut self, cx: &mut Context<Self>) {
        let panel = self.right_terminal_panel(cx);
        let opened = panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.open_tab_for_selected(cx)
        });
        if let Some(tab) = opened {
            let key = self.panel_key(cx);
            self.right_tabs
                .entry(key)
                .or_default()
                .push(RightSurface::Terminal(tab));
            self.set_right_active(RightSurface::Terminal(tab), cx);
        }
    }
}
