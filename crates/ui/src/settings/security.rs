//! Settings → Security: the default agent access level for new chats.
//!
//! The composer remains the per-chat override; this page only sets the
//! persisted default that seeds `AppState.access_mode` at boot.
//!
//! Same pattern as [`crate::settings::composer::ComposerDefaults`]:
//! a small JSON file beside `ui-settings.json`, corrupt-file-tolerant load,
//! atomic temp+rename save.

use std::io;
use std::path::{Path, PathBuf};

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};
use serde::{Deserialize, Serialize};

use komet_proto::SandboxLevel;

use crate::settings::widgets;
use crate::theme::Theme;

const FILE_NAME: &str = "security-defaults.json";

/// Persisted security defaults (V1: default sandbox level only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SecurityDefaults {
    pub default_sandbox: SandboxLevel,
}

impl Default for SecurityDefaults {
    fn default() -> Self {
        Self {
            default_sandbox: SandboxLevel::WorkspaceWrite,
        }
    }
}

impl SecurityDefaults {
    /// Load from `{data_dir}/security-defaults.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<SecurityDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "security-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

pub fn level_title(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "Read only",
        SandboxLevel::WorkspaceWrite => "Sandboxed",
        SandboxLevel::DangerFullAccess => "Full access",
    }
}

pub fn level_description(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "The agent can read files but cannot make changes.",
        SandboxLevel::WorkspaceWrite => {
            "The agent can edit files inside the workspace. System access stays sandboxed."
        }
        SandboxLevel::DangerFullAccess => {
            "Unrestricted access: the agent can run anything, anywhere. Use with care."
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SecurityEvent {
    DefaultSandboxChanged(SandboxLevel),
}

pub struct SecurityPage {
    level: SandboxLevel,
}

impl EventEmitter<SecurityEvent> for SecurityPage {}

impl SecurityPage {
    pub fn new(level: SandboxLevel, _cx: &mut Context<Self>) -> Self {
        Self { level }
    }

    fn pick(&mut self, level: SandboxLevel, cx: &mut Context<Self>) {
        if self.level != level {
            self.level = level;
            cx.emit(SecurityEvent::DefaultSandboxChanged(level));
            cx.notify();
        }
    }

    fn row(
        &self,
        theme: &Theme,
        level: SandboxLevel,
        first: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.level == level;
        let dot = div()
            .size(px(16.0))
            .flex_none()
            .rounded_full()
            .border_2()
            .border_color(if selected { theme.accent } else { theme.border })
            .flex()
            .items_center()
            .justify_center()
            .when(selected, |el| {
                el.child(div().size(px(8.0)).rounded_full().bg(theme.accent))
            });
        widgets::card_row(theme, first)
            .cursor_pointer()
            .id(match level {
                SandboxLevel::ReadOnly => "security-default-readonly",
                SandboxLevel::WorkspaceWrite => "security-default-sandboxed",
                SandboxLevel::DangerFullAccess => "security-default-full-access",
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pick(level, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .pr(px(12.0))
                    .child(dot),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(widgets::row_title(theme, level_title(level)))
                            .when(level == SandboxLevel::DangerFullAccess, |el| {
                                el.child(widgets::badge(theme, "Caution"))
                            }),
                    )
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            div()
                                .child(SharedString::from(level_description(level).to_string()))
                                .into_any_element(),
                        ],
                    )),
            )
            .into_any_element()
    }
}

impl Render for SecurityPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        widgets::page_column()
            .child(widgets::page_header(&theme, "Security", None))
            .child(widgets::page_subtitle(
                &theme,
                "Default agent access for new chats. The composer can still override it per chat.",
            ))
            .child(widgets::field_label(&theme, "Default access"))
            .child(
                widgets::section_card(&theme)
                    .child(self.row(&theme, SandboxLevel::ReadOnly, true, cx))
                    .child(self.row(&theme, SandboxLevel::WorkspaceWrite, false, cx))
                    .child(self.row(&theme, SandboxLevel::DangerFullAccess, false, cx)),
            )
            .when(self.level == SandboxLevel::DangerFullAccess, |el| {
                el.child(widgets::warning_strip(
                    &theme,
                    "Full access lets the agent run unsandboxed commands on this device.",
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = SecurityDefaults {
            default_sandbox: SandboxLevel::DangerFullAccess,
        };
        defaults.save(dir.path()).unwrap();
        assert_eq!(SecurityDefaults::load(dir.path()), defaults);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            SecurityDefaults::load(dir.path()),
            SecurityDefaults::default()
        );
        std::fs::write(SecurityDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            SecurityDefaults::load(dir.path()),
            SecurityDefaults::default()
        );
    }

    #[test]
    fn default_is_workspace_write() {
        assert_eq!(
            SecurityDefaults::default().default_sandbox,
            SandboxLevel::WorkspaceWrite
        );
    }
}
