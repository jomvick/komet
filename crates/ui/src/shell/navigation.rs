use super::*;
use crate::icons;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    /// One card per agent: enablement, CLI install state and its logins.
    Providers,
    Appearance,
    Files,
    Notifications,
    Shortcuts,
    Archived,
    Sync,
    Security,
    Integrations,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 10] = [
        SettingsSection::Devices,
        SettingsSection::Providers,
        SettingsSection::Appearance,
        SettingsSection::Files,
        SettingsSection::Notifications,
        SettingsSection::Shortcuts,
        SettingsSection::Archived,
        SettingsSection::Sync,
        SettingsSection::Security,
        SettingsSection::Integrations,
    ];

    /// Sidebar + header label (komet settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::Providers => "Providers",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Files => "Files",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Archived sessions",
            SettingsSection::Sync => "Sync",
            SettingsSection::Security => "Security",
            SettingsSection::Integrations => "Integrations",
        }
    }

    /// Outline glyphs identify distinct destinations. The selected destination
    /// receives a solid tile in the navigation, keeping hierarchy without
    /// reusing one generic settings icon for unrelated pages.
    pub const fn nav_icon(self) -> &'static str {
        match self {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::Providers => icons::WIDGET,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Files => icons::FOLDER_WITH_FILES,
            SettingsSection::Notifications => icons::BELL,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
            SettingsSection::Sync => icons::CLOUD,
            SettingsSection::Security => icons::SHIELD,
            SettingsSection::Integrations => icons::PLUG,
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

/// One right-pane surface tab: a workspace browser, an individual workspace
/// file editor, a git-diff page, an embedded terminal, a subagent transcript,
/// `Picker` is the empty surface chooser.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RightSurface {
    #[default]
    Picker,
    Files,
    File(u64),
    Diff(u64),
    Terminal(u64),
    /// A subagent's transcript, read-only (per-subagent viz) — the handle
    /// keys [`Shell::subagent_tabs`].
    Subagent(u64),
}

pub fn push_unique_right_surface(tabs: &mut Vec<RightSurface>, surface: RightSurface) -> bool {
    if tabs.contains(&surface) {
        false
    } else {
        tabs.push(surface);
        true
    }
}

pub fn workspace_file_title(path: &str) -> SharedString {
    path.rsplit('/').next().unwrap_or(path).to_string().into()
}

/// Per-chat panel open flags (komet parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global).
///
/// Everything defaults CLOSED — the right pane included (user request,
/// revising the earlier default-open: it popped open on every session you
/// visited). Opening is an explicit act, remembered per chat for the rest of
/// the app run; a fresh open with no surface tabs lands on the picker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    /// Right pane visible (the surface host — historically the Changes pane).
    pub changes_open: bool,
    /// Which surface tab renders; validated against the live tab list each
    /// frame (a closed tab falls back gracefully).
    pub right_active: RightSurface,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }

    /// Mutate `key`'s flags in place (right-pane surface bookkeeping).
    pub fn update(&mut self, key: &str, f: impl FnOnce(&mut ChatPanels)) {
        f(self.map.entry(key.to_string()).or_default());
    }
}

/// One route-history entry (komet parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (komet window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (komet's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (komet window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Estimated sidebar row height for the resort diff (title line 17px inside
/// 6px vertical padding + the location subline's 14px line + 2px gap — Active
/// rows always carry the folder · device subline).
/// Session row height (FLIP estimate): space line + title + meta line
/// (harness mark, plus branch for worktrees).
pub const CHAT_ROW_HEIGHT: f32 = 61.0;
/// Flex gap between sidebar list items.
pub const SIDEBAR_LIST_GAP: f32 = 2.0;

/// Ramp height of the sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
pub const SIDEBAR_GLASS_FADE_BAND: f32 = 32.0;

impl Shell {
    // ---- routes / settings ----

    /// Close the user menu through the exit animation (no-op when closed).
    pub(super) fn close_user_menu(&mut self, cx: &mut Context<Self>) {
        if self.user_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.user_menu);
            cx.notify();
        }
    }

    /// Close the session-row context menu through the exit animation.
    pub(super) fn close_chat_menu(&mut self, cx: &mut Context<Self>) {
        if self.chat_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.chat_menu);
            cx.notify();
        }
    }

    pub(super) fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        // Recreate per visit: the page's ListHarnesses load re-probes which
        // CLIs are installed, so installing one shows up on the next open.
        if section == SettingsSection::Providers {
            self.providers_page = None;
        }
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    pub(super) fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    pub(super) fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    pub(super) fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    pub(super) fn settings_outlet(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Providers => {
                if self.providers_page.is_none() {
                    let state = self.state.clone();
                    self.providers_page = Some(cx.new(|cx| ProvidersPage::new(state, cx)));
                }
                match &self.providers_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Files => {
                if self.files_settings_page.is_none() {
                    let page = cx.new(|cx| {
                        FilesSettingsPage::new(
                            self.settings.files_autosave_delay_ms,
                            self.settings.files_editor_font_size,
                            self.settings.files_word_wrap,
                            self.settings.files_show_all,
                            cx,
                        )
                    });
                    // Persist whenever the page flips a setting.
                    let _sub = cx.subscribe(
                        &page,
                        |this: &mut Shell,
                         _,
                         event: &crate::settings::files::FilesSettingsEvent,
                         cx| {
                            use crate::settings::files::FilesSettingsEvent;
                            match *event {
                                FilesSettingsEvent::AutosaveDelayChanged(ms) => {
                                    this.settings.files_autosave_delay_ms = ms;
                                    this.schedule_save(cx);
                                }
                                FilesSettingsEvent::EditorFontSizeChanged(size) => {
                                    this.set_files_editor_font_size(size, cx);
                                }
                                FilesSettingsEvent::WordWrapChanged(_) => {
                                    // Propagated from FilesSurface events; handled via
                                    // set_files_word_wrap when triggered from a surface.
                                }
                                FilesSettingsEvent::ShowAllFilesChanged(_) => {
                                    // Propagated from FilesSurface events.
                                }
                            }
                            cx.notify();
                        },
                    );
                    self.files_settings_page = Some(page);
                }
                match &self.files_settings_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let page = cx.new(|cx| {
                        NotificationsPage::new(
                            self.settings.sound_enabled,
                            self.settings.notifications_enabled,
                            self.settings.notifications_background_only,
                            cx,
                        )
                    });
                    // Persist the flags whenever the page flips one.
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::Changed {
                                sound,
                                desktop,
                                background_only,
                            } = *event;
                            this.settings.sound_enabled = sound;
                            this.settings.notifications_enabled = desktop;
                            this.settings.notifications_background_only = background_only;
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Sync => {
                if self.sync_page.is_none() {
                    let state = self.state.clone();
                    let page = cx.new(|cx| SyncPage::new(state, cx));
                    self.sync_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &SyncEvent, cx| {
                            let SyncEvent::OpenWorkspace(env_id) = event;
                            let env_id = env_id.clone();
                            this.close_settings(cx);
                            this.select_workspace(&env_id, cx);
                        },
                    ));
                    self.sync_page = Some(page);
                }
                match &self.sync_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Security => {
                if self.security_page.is_none() {
                    let level = self
                        .state
                        .read(cx)
                        .data_dir
                        .as_deref()
                        .map(SecurityDefaults::load)
                        .unwrap_or_default()
                        .default_sandbox;
                    let page = cx.new(|cx| SecurityPage::new(level, cx));
                    self.security_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &SecurityEvent, cx| {
                            let SecurityEvent::DefaultSandboxChanged(level) = *event;
                            if let Some(data_dir) = this.state.read(cx).data_dir.clone() {
                                let defaults = SecurityDefaults {
                                    default_sandbox: level,
                                };
                                if let Err(err) = defaults.save(&data_dir) {
                                    tracing::warn!(error = %err, "security-defaults save failed");
                                }
                            }
                            this.state.update(cx, |s, _| {
                                // The default only seeds NEW chats; the level of
                                // the chat currently open (if any) is its own.
                                s.set_new_chat_access(level);
                            });
                            cx.notify();
                        },
                    ));
                    self.security_page = Some(page);
                }
                match &self.security_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Integrations => {
                if self.integrations_page.is_none() {
                    let state = self.state.clone();
                    self.integrations_page = Some(cx.new(|cx| IntegrationsPage::new(state, cx)));
                }
                match &self.integrations_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }
}

/// Navigation memory: what the active device last looked at (session or new-task).
impl Shell {
    /// Record the current navigation for the active device (session or new-task).
    pub(super) fn remember_current_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(device_id) = self.state.read(cx).local_device_id.clone() else {
            return;
        };
        let nav = if self.active_chat.is_empty() {
            let state = self.state.read(cx);
            let project_id = if state.no_project {
                None
            } else {
                state.selected_space.clone()
            };
            RememberedNavigation::NewTask { project_id }
        } else {
            RememberedNavigation::Session {
                id: self.active_chat.clone(),
            }
        };
        if self.settings.last_session_by_device.get(&device_id) != Some(&nav) {
            self.settings.last_session_by_device.insert(device_id, nav);
            self.schedule_save(cx);
        }
    }

    /// Prune stale remembered entries (archived/deleted sessions, deleted projects)
    /// — same rule as `open_tabs` pruning against the doc.
    pub(super) fn prune_remembered_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        let mut to_remove = Vec::new();
        for (device_id, nav) in &self.settings.last_session_by_device {
            let keep = match nav {
                RememberedNavigation::Session { id } => {
                    let state = self.state.read(cx);
                    let chat = state.chats.iter().find(|c| &c.id == id);
                    match chat {
                        Some(c) if !c.archived => match &c.space_id {
                            Some(sid) => state.space_row(sid).is_some(),
                            None => true,
                        },
                        _ => false,
                    }
                }
                RememberedNavigation::NewTask { project_id } => match project_id {
                    None => true,
                    Some(pid) => self.state.read(cx).space_row(pid).is_some(),
                },
            };
            if !keep {
                to_remove.push(device_id.clone());
            }
        }
        let pruned = !to_remove.is_empty();
        for id in to_remove {
            self.settings.last_session_by_device.remove(&id);
        }
        // Drop folder expansion state for deleted spaces (same rule as the
        // remembered destinations above).
        let before = self.settings.expanded_spaces.len();
        let state = self.state.read(cx);
        self.settings
            .expanded_spaces
            .retain(|id| state.space_row(id).is_some());
        pruned || self.settings.expanded_spaces.len() != before
    }
}
