//! The right-pane "Files" content: an interactive project file explorer
//! and filter/search viewer.
//!
//! Features:
//! - Rooted in the active session's or space's project directory (`cwd`).
//! - Collapsible directories with lazy recursive folder loading (`ListFolders`).
//! - Instant fuzzy search / filtering (`SearchFiles`).
//! - File path copying to clipboard.
//! - Polished design matching the Komet UI aesthetics and theme tokens.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use komet_proto::{FileSearchMatch, FolderListing};
use komet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

/// A single node in the file explorer tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    pub name: String,
    pub path: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub is_repo: bool,
    pub depth: usize,
}

pub struct FilesPanel {
    state: gpui::Entity<AppState>,
    focus: FocusHandle,
    search: Entity<ComposerInput>,
    _search_sub: Subscription,
    search_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    sub_tasks: HashMap<String, Task<()>>,

    // State
    root_path: Option<String>,
    device_id: Option<String>,
    root_entries: Vec<FileNode>,
    sub_entries: HashMap<String, Vec<FileNode>>,
    expanded_paths: HashSet<String>,
    loading_paths: HashSet<String>,
    loading_root: bool,
    error: Option<String>,

    // Search results when filter query is non-empty
    search_query: String,
    search_matches: Option<Vec<FileSearchMatch>>,
    search_loading: bool,

    // Feedback
    copied_path: Option<(String, Instant)>,
    scroll_handle: ScrollHandle,
}

impl FilesPanel {
    pub fn new(state: gpui::Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let search = cx.new(|cx| ComposerInput::new("Filter files…", cx));
        let _search_sub = cx.subscribe(&search, |this, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                let query = this.search.read(cx).text().to_string();
                this.on_filter_changed(query, cx);
            }
            ComposerInputEvent::Submitted => {
                cx.notify();
            }
            _ => {}
        });

        let mut panel = Self {
            state,
            focus,
            search,
            _search_sub,
            search_task: None,
            load_task: None,
            sub_tasks: HashMap::new(),
            root_path: None,
            device_id: None,
            root_entries: Vec::new(),
            sub_entries: HashMap::new(),
            expanded_paths: HashSet::new(),
            loading_paths: HashSet::new(),
            loading_root: false,
            error: None,
            search_query: String::new(),
            search_matches: None,
            search_loading: false,
            copied_path: None,
            scroll_handle: ScrollHandle::new(),
        };

        panel.ensure_root(cx);
        panel
    }

    pub fn tab_title(&self) -> SharedString {
        if let Some(root) = &self.root_path {
            if let Some(name) = Path::new(root).file_name().and_then(|n| n.to_str()) {
                return SharedString::from(format!("Files · {name}"));
            }
        }
        SharedString::from("Files")
    }

    /// Resolve root path and target device from current state and initiate loading.
    pub fn ensure_root(&mut self, cx: &mut Context<Self>) {
        let resolved = {
            let state = self.state.read(cx);
            if let Some(chat) = state.selected_chat_row() {
                let cwd = chat.cwd.clone();
                let device = Some(chat.device_id.clone());
                cwd.map(|c| (c, device))
            } else if let Some(space) = state.selected_space_row() {
                Some((space.path.clone(), Some(space.device_id.clone())))
            } else {
                None
            }
        };

        if let Some((root, device)) = resolved {
            let changed =
                self.root_path.as_deref() != Some(root.as_str()) || self.device_id != device;
            if changed || self.root_entries.is_empty() {
                self.root_path = Some(root);
                self.device_id = device;
                self.expanded_paths.clear();
                self.sub_entries.clear();
                self.load_root(cx);
            }
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_root(cx);
    }

    fn load_root(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root_path.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };

        let device_id = self.device_id.clone();
        let local = self.state.read(cx).local_device_id.clone();
        self.loading_root = true;
        self.error = None;

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("path".into(), serde_json::Value::String(root.clone()));
            if let (Some(target), Some(local_id)) = (&device_id, &local)
                && local_id != target
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }

            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;

            this.update(cx, |panel, cx| {
                panel.loading_root = false;
                match result {
                    Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                        Ok(listing) => {
                            let base = listing.path.trim_end_matches('/').to_string();
                            let mut entries: Vec<FileNode> = listing
                                .entries
                                .into_iter()
                                .map(|e| {
                                    let full = format!("{base}/{}", e.name);
                                    FileNode {
                                        name: e.name.clone(),
                                        path: full,
                                        rel_path: e.name,
                                        is_dir: e.is_dir,
                                        is_repo: e.is_repo,
                                        depth: 0,
                                    }
                                })
                                .collect();
                            panel.sort_nodes(&mut entries);
                            panel.root_entries = entries;
                        }
                        Err(err) => panel.error = Some(err.to_string()),
                    },
                    Err(err) => panel.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle_folder(
        &mut self,
        path: String,
        rel_path: String,
        depth: usize,
        cx: &mut Context<Self>,
    ) {
        if self.expanded_paths.contains(&path) {
            self.expanded_paths.remove(&path);
            cx.notify();
            return;
        }

        self.expanded_paths.insert(path.clone());
        if !self.sub_entries.contains_key(&path) {
            self.load_sub_folder(path, rel_path, depth + 1, cx);
        }
        cx.notify();
    }

    fn load_sub_folder(
        &mut self,
        dir_path: String,
        rel_prefix: String,
        child_depth: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let device_id = self.device_id.clone();
        let local = self.state.read(cx).local_device_id.clone();
        self.loading_paths.insert(dir_path.clone());

        let path_key = dir_path.clone();
        let task_key = dir_path.clone();

        let task = cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("path".into(), serde_json::Value::String(dir_path.clone()));
            if let (Some(target), Some(local_id)) = (&device_id, &local)
                && local_id != target
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }

            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;

            this.update(cx, |panel, cx| {
                panel.loading_paths.remove(&path_key);
                if let Ok(value) = result
                    && let Ok(listing) = serde_json::from_value::<FolderListing>(value)
                {
                    let mut nodes: Vec<FileNode> = listing
                        .entries
                        .into_iter()
                        .map(|e| {
                            let full = format!("{}/{}", listing.path.trim_end_matches('/'), e.name);
                            let rel = format!("{}/{}", rel_prefix.trim_end_matches('/'), e.name);
                            FileNode {
                                name: e.name,
                                path: full,
                                rel_path: rel,
                                is_dir: e.is_dir,
                                is_repo: e.is_repo,
                                depth: child_depth,
                            }
                        })
                        .collect();
                    panel.sort_nodes(&mut panel.nodes_copy(&mut nodes));
                    panel.sub_entries.insert(path_key, nodes);
                }
                cx.notify();
            })
            .ok();
        });

        self.sub_tasks.insert(task_key, task);
        cx.notify();
    }

    fn nodes_copy<'a>(&self, nodes: &'a mut [FileNode]) -> &'a mut [FileNode] {
        nodes
    }

    fn sort_nodes(&self, nodes: &mut [FileNode]) {
        nodes.sort_by(|a, b| {
            // Directories first, then files alphabetically (case-insensitive)
            match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            }
        });
    }

    fn on_filter_changed(&mut self, query: String, cx: &mut Context<Self>) {
        let trimmed = query.trim().to_string();
        self.search_query = trimmed.clone();

        if trimmed.is_empty() {
            self.search_matches = None;
            self.search_loading = false;
            self.search_task = None;
            cx.notify();
            return;
        }

        let Some(root) = self.root_path.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };

        let device_id = self.device_id.clone();
        let local = self.state.read(cx).local_device_id.clone();
        self.search_loading = true;

        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(60))
                .await;

            let mut params = serde_json::Map::new();
            params.insert("root".into(), serde_json::Value::String(root));
            params.insert("query".into(), serde_json::Value::String(trimmed));
            params.insert("featuredPaths".into(), serde_json::Value::Array(Vec::new()));
            if let (Some(target), Some(local_id)) = (&device_id, &local)
                && local_id != target
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }

            let result = engine
                .client()
                .call(methods::SEARCH_FILES, serde_json::Value::Object(params))
                .await;

            this.update(cx, |panel, cx| {
                panel.search_loading = false;
                if let Ok(value) = result
                    && let Ok(matches) = serde_json::from_value::<Vec<FileSearchMatch>>(value)
                {
                    panel.search_matches = Some(matches);
                } else {
                    panel.search_matches = Some(Vec::new());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_file_click(&mut self, rel_path: &str, _window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel_path.to_string()));
        self.copied_path = Some((rel_path.to_string(), Instant::now()));
        cx.notify();
    }

    /// Flatten current visible tree into renderable row items.
    fn visible_tree_rows(&self) -> Vec<FileNode> {
        let mut rows = Vec::new();
        self.collect_visible_rows(&self.root_entries, &mut rows);
        rows
    }

    fn collect_visible_rows(&self, nodes: &[FileNode], out: &mut Vec<FileNode>) {
        for node in nodes {
            out.push(node.clone());
            if node.is_dir && self.expanded_paths.contains(&node.path) {
                if let Some(children) = self.sub_entries.get(&node.path) {
                    self.collect_visible_rows(children, out);
                }
            }
        }
    }

    // ── Render ───────────────────────────────────────────────────────────────

    pub fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let text_muted = theme.text_muted;
        let border = theme.border;
        let bg = theme.bg;

        let root_label = self
            .root_path
            .as_deref()
            .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
            .unwrap_or("Project");

        div()
            .w_full()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(border)
            .bg(bg)
            .p(px(8.0))
            .gap(px(8.0))
            // Project root bar
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                icon(icons::FOLDER_WITH_FILES)
                                    .size(px(14.0))
                                    .text_color(theme.accent),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(SharedString::from(root_label)),
                            ),
                    )
                    .child(
                        div()
                            .id("files-refresh-btn")
                            .p(px(3.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(cx);
                            }))
                            .child(icon(icons::REFRESH).size(px(13.0)).text_color(text_muted)),
                    ),
            )
            // Filter search input
            .child(
                div()
                    .w_full()
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(icon(icons::MAGNIFER).size(px(13.0)).text_color(text_muted))
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .into_any_element()
    }

    fn render_node_row(
        &self,
        node: &FileNode,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_expanded = self.expanded_paths.contains(&node.path);
        let is_loading = self.loading_paths.contains(&node.path);
        let text_color = if node.is_dir {
            theme.text
        } else {
            theme.text_muted
        };
        let node_path = node.path.clone();
        let rel_path = node.rel_path.clone();
        let depth = node.depth;
        let is_dir = node.is_dir;

        let indent = px(14.0 * depth as f32 + 6.0);

        let chevron_icon = if is_expanded {
            icons::ALT_ARROW_DOWN
        } else {
            icons::ALT_ARROW_RIGHT
        };

        let node_icon = if node.is_dir {
            if is_expanded {
                icons::FOLDER_WITH_FILES
            } else {
                icons::FOLDER
            }
        } else {
            icons::DOCUMENT
        };

        let row_id: SharedString = format!("file-row-{}", node.path).into();

        let is_just_copied = self
            .copied_path
            .as_ref()
            .is_some_and(|(p, t)| p == &rel_path && t.elapsed() < Duration::from_secs(2));

        div()
            .id(row_id)
            .w_full()
            .h(px(26.0))
            .pl(indent)
            .pr(px(8.0))
            .rounded(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                if is_dir {
                    this.toggle_folder(node_path.clone(), rel_path.clone(), depth, cx);
                } else {
                    this.on_file_click(&rel_path, window, cx);
                }
            }))
            // Chevron or spacer
            .child(
                div()
                    .w(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(is_dir, |d| {
                        d.child(
                            icon(chevron_icon)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        )
                    }),
            )
            // Icon
            .child(icon(node_icon).size(px(14.0)).text_color(if is_dir {
                theme.accent
            } else {
                theme.text_muted
            }))
            // Name
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(text_color)
                    .line_clamp(1)
                    .child(SharedString::from(node.name.clone())),
            )
            // Copied badge or loading indicator
            .when(is_loading, |d| {
                d.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("…")),
                )
            })
            .when(is_just_copied, |d| {
                d.child(
                    div()
                        .px(px(4.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.element_active)
                        .text_size(px(9.5))
                        .text_color(theme.accent)
                        .child(SharedString::from("Copied")),
                )
            })
            .into_any_element()
    }

    fn render_search_match_row(
        &self,
        m: &FileSearchMatch,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rel_path = m.path.clone();
        let is_dir = m.is_dir;
        let is_just_copied = self
            .copied_path
            .as_ref()
            .is_some_and(|(p, t)| p == &rel_path && t.elapsed() < Duration::from_secs(2));

        let file_icon = if is_dir {
            icons::FOLDER
        } else {
            icons::DOCUMENT
        };

        let row_id: SharedString = format!("search-match-{}", m.path).into();

        div()
            .id(row_id)
            .w_full()
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.on_file_click(&rel_path, window, cx);
            }))
            .child(icon(file_icon).size(px(14.0)).text_color(if is_dir {
                theme.accent
            } else {
                theme.text_muted
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .line_clamp(1)
                    .child(SharedString::from(m.path.clone())),
            )
            .when(is_just_copied, |d| {
                d.child(
                    div()
                        .px(px(4.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.element_active)
                        .text_size(px(9.5))
                        .text_color(theme.accent)
                        .child(SharedString::from("Copied")),
                )
            })
            .into_any_element()
    }
}

impl Focusable for FilesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;

        // Auto-heal if root hasn't been loaded
        if self.root_path.is_none() || self.root_entries.is_empty() && !self.loading_root {
            self.ensure_root(cx);
        }

        let is_searching = !self.search_query.is_empty();

        let content: AnyElement = if self.loading_root {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("Loading project files…"))
                .into_any_element()
        } else if let Some(error) = &self.error {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p(px(16.0))
                .gap(px(8.0))
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(20.0))
                        .text_color(theme.danger),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .text_center()
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("files-retry-btn")
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(11.5))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if is_searching {
            if self.search_loading {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Searching files…"))
                    .into_any_element()
            } else if let Some(matches) = &self.search_matches {
                if matches.is_empty() {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("No files matching query"))
                        .into_any_element()
                } else {
                    let match_elements: Vec<AnyElement> = matches
                        .iter()
                        .map(|m| self.render_search_match_row(m, &theme, cx))
                        .collect();
                    div()
                        .id("files-search-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .p(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .children(match_elements)
                        .into_any_element()
                }
            } else {
                gpui::Empty.into_any_element()
            }
        } else if self.root_entries.is_empty() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("No files found in workspace"))
                .into_any_element()
        } else {
            let tree_rows = self.visible_tree_rows();
            let row_elements: Vec<AnyElement> = tree_rows
                .iter()
                .map(|node| self.render_node_row(node, &theme, cx))
                .collect();

            div()
                .id("files-tree-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .p(px(6.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(row_elements)
                .into_any_element()
        };

        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .child(self.render_header(&theme, cx))
            .child(div().flex_1().min_h_0().child(content))
    }
}
