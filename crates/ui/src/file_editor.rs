//! Right-pane file editor surface tab.
//!
//! Provides a simple text editor for viewing and editing file contents,
//! with dirty-state tracking for VS Code-style preview tab behavior.

use std::path::Path;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, KeyBinding, SharedString,
    Subscription, Task, actions, div, prelude::*, px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::settings::platform_combo;
use crate::state::AppState;
use crate::theme::Theme;

actions!(file_editor, [SaveFile]);

/// Registers the `mod-s` → `SaveFile` binding under the "FileEditor" key
/// context (own `actions!`, mirrors `composer::init`'s "PaletteSearch"
/// pattern for a scoped override). `render` pushes "FileEditor" as an
/// EXTRA context alongside the inner `ComposerInput`'s own "Composer"
/// context — both are active at once at the focused node, so this doesn't
/// need to redeclare any text-editing bindings; it only adds the one this
/// surface needs on top. Being a more specific (non-`None`) context than
/// the global `mod-s` → `ToggleSidebar` binding, it takes precedence while
/// focus is inside the file editor.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        &platform_combo("mod-s"),
        SaveFile,
        Some("FileEditor"),
    )]);
}

/// A file editor surface tab in the right pane.
pub struct FileEditorPanel {
    state: gpui::Entity<AppState>,
    focus: FocusHandle,
    file_path: String,
    content: String,
    dirty: bool,
    pinned: bool,
    loading: bool,
    error: Option<String>,
    editor: Entity<ComposerInput>,
    _editor_sub: Subscription,
    save_task: Option<Task<()>>,
    highlighted: Option<komet_syntax::HighlightedDocument>,
}

impl FileEditorPanel {
    pub fn new(state: Entity<AppState>, file_path: String, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let editor = cx.new(|cx| {
            let mut input = ComposerInput::new("File editor…", cx);
            // Lift the chat composer's ~240px autogrow ceiling — a file
            // editor needs to fill its flex_1 pane and scroll internally,
            // not clip to a chat-message-sized box (see the field's doc
            // comment on `ComposerInput` for how the large value here still
            // ends up bounded by the real pane height).
            input.set_max_content_height(f32::MAX, cx);
            input
        });

        let editor_for_sub = editor.clone();
        let _editor_sub = cx.subscribe(&editor, move |this, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                let new_content = editor_for_sub.read(cx).text().to_string();
                if new_content != this.content {
                    this.dirty = true;
                    this.update_tab_title(cx);
                }
            }
            ComposerInputEvent::Submitted => {
                this.save_file(cx);
            }
            _ => {}
        });

        let mut panel = Self {
            state,
            focus,
            file_path: file_path.clone(),
            content: String::new(),
            dirty: false,
            pinned: false,
            loading: false,
            error: None,
            editor,
            _editor_sub,
            save_task: None,
            highlighted: None,
        };

        panel.load_file(file_path, cx);
        panel
    }

    /// Load a file's content into the editor.
    pub fn load_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.file_path = path.clone();
        self.dirty = false;
        self.loading = true;
        self.error = None;
        cx.notify();

        let path_for_task = path.clone();

        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = std::fs::read_to_string(&path_for_task).map_err(|e| e.to_string());

            this.update(cx, |panel, cx| {
                match result {
                    Ok(content) => {
                        let doc = komet_syntax::highlight(komet_syntax::HighlightRequest{
                            source: &content, path: Some(&path_for_task), fence_tag: None,
                        }).ok();
                        panel.highlighted = doc.clone();
                        panel.content = content.clone();
                        panel.editor.update(cx, |editor, cx| {
                            // set_text first: it resets projection/undo state,
                            // which would otherwise clobber the doc we're
                            // about to attach.
                            editor.set_text(content, cx);
                            editor.set_syntax_doc(doc, cx);
                        });
                    }
                    Err(_err) => {
                        panel.highlighted = None;
                        panel.editor.update(cx, |editor, cx| {
                            editor.set_text(format!("// Error loading file: {}", path_for_task), cx);
                            editor.set_syntax_doc(None, cx);
                        });
                    }
                }
                panel.loading = false;
                panel.dirty = false;
                panel.update_tab_title(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Save the current content back to the file system.
    pub fn save_file(&mut self, cx: &mut Context<Self>) {
        if !self.dirty || self.loading {
            return;
        }

        self.loading = true;
        cx.notify();

        let content = self.editor.read(cx).text().to_string();
        let path = self.file_path.clone();

        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = std::fs::write(&path, &content).map_err(|e| e.to_string());

            this.update(cx, |panel, cx| {
                panel.loading = false;
                if result.is_ok() {
                    panel.dirty = false;
                    panel.content = content;
                    panel.update_tab_title(cx);
                } else {
                    panel.error = Some("Failed to save file".to_string());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Ensure the file content is loaded (called when tab becomes active).
    pub fn ensure_content(&mut self, _cx: &mut Context<Self>) {
        // Content loading is handled in new() and load_file()
        // This method exists for parity with other surface tabs
    }

    /// Check if the editor has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn is_pinned(&self) -> bool { self.pinned }
    pub fn pin(&mut self) { self.pinned = true; }
    pub fn highlighted(&self) -> Option<&komet_syntax::HighlightedDocument> { self.highlighted.as_ref() }

    /// Get the current file path.
    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    /// Get the tab title (file name with dirty indicator).
    pub fn tab_title(&self) -> SharedString {
        let name = Path::new(&self.file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.file_path);

        let mut title = if self.dirty {
            format!("{} ●", name)
        } else {
            name.to_string()
        };
        if !self.pinned && !self.dirty {
            title = format!("{} (preview)", title);
        }
        title.into()
    }

    /// Update the tab title in the shell.
    fn update_tab_title(&self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Set the editor text content.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.editor.update(cx, |editor, cx| {
            editor.set_text(self.content.clone(), cx);
        });
    }

    /// Render the file editor.
    pub fn render(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let bg = theme.bg;
        let border = theme.border;
        let text = theme.text;
        let text_muted = theme.text_muted;

        let title = self.tab_title();
        let is_dirty = self.dirty;
        let loading = self.loading;
        let error = self.error.clone();
        let file_path = self.file_path.clone();
        // suppress dead_code until AppState is wired to editor features
        let _ = &self.state;
        let _ = &self.focus;

        div()
            .w_full()
            .h_full()
            .key_context("FileEditor")
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| {
                this.save_file(cx);
            }))
            .flex()
            .flex_col()
            .bg(bg)
            .child(
                // Toolbar
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b(px(1.0))
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .size(px(16.0))
                                    .flex_none()
                                    .child(icon(icons::DOCUMENT).size(px(14.0)).text_color(text_muted))
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(text)
                                    .child(title.clone())
                            )
                            .when(is_dirty, |el| {
                                el.child(
                                    div()
                                        .w(px(6.0))
                                        .h(px(6.0))
                                        .rounded(px(3.0))
                                        .bg(text_muted)
                                )
                            })
                    ),
            )
            .when_some(error.clone(), |el, msg| {
                el.child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .bg(gpui::rgb(0x7f1d1d))
                        .text_color(gpui::rgb(0xfecaca))
                        .text_size(px(11.0))
                        .child(format!("Error: {msg}")),
                )
            })
            .when(loading, |el| {
                el.child(
                    div()
                        .px(px(12.0))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(text_muted)
                        .child("Loading…"),
                )
            })
            .child(
                div()
                    .px(px(12.0))
                    .py(px(2.0))
                    .text_size(px(10.0))
                    .text_color(text_muted)
                    .child(file_path.clone()),
            )
            .child(
                // Content area — directement éditable, scroll interne géré par
                // ComposerInput (scroll_top + ContentMask). Le parent doit rester
                // flex_1 + min_h_0 + overflow_hidden pour que ComposerTextElement
                // reçoive un available.height défini et clamp son height au viewport
                // (sinon input_max_scroll = 0 et la molette ne bouge rien).
                // Défilement : molette / drag-sélection / flèches. Pas de scrollbar
                // native GPUI ici — le scroll est manuel (voir composer.rs:2415).
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.editor.clone().into_any_element()),
            )
            .into_any_element()
    }
}

impl Focusable for FileEditorPanel {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}
