//! Settings → General: Devices, Security, Files and Memory on a single
//! scrolling page. The four sections keep their own pages and state — they
//! render here in embedded mode (no page chrome, no nested scroller) under a
//! shared header and a chip row that jumps to each section.

use gpui::{AnyElement, Context, Entity, ScrollHandle, Window, div, prelude::*, px};

use super::devices::DevicesPage;
use super::files::FilesSettingsPage;
use super::memory::MemoryPage;
use super::security::SecurityPage;
use super::widgets;
use crate::shell::GeneralAnchor;
use crate::theme::Theme;

pub struct GeneralPage {
    devices: Entity<DevicesPage>,
    security: Entity<SecurityPage>,
    files: Entity<FilesSettingsPage>,
    memory: Entity<MemoryPage>,
    scroll: ScrollHandle,
    /// Anchor requested by a chip click or `Shell::open_general`, applied on
    /// the next frame once the scroll handle knows the children bounds.
    pending_anchor: Option<GeneralAnchor>,
    /// Chip rendered active — the last anchor the user jumped to.
    active_anchor: Option<GeneralAnchor>,
}

impl GeneralPage {
    pub fn new(
        devices: Entity<DevicesPage>,
        security: Entity<SecurityPage>,
        files: Entity<FilesSettingsPage>,
        memory: Entity<MemoryPage>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            devices,
            security,
            files,
            memory,
            scroll: ScrollHandle::new(),
            pending_anchor: None,
            active_anchor: None,
        }
    }

    /// Scroll so `anchor`'s section is visible and mark its chip active.
    pub fn scroll_to(&mut self, anchor: GeneralAnchor, cx: &mut Context<Self>) {
        self.pending_anchor = Some(anchor);
        self.active_anchor = Some(anchor);
        cx.notify();
    }

    fn section(
        &self,
        theme: &Theme,
        anchor: GeneralAnchor,
        subtitle: &'static str,
        page: AnyElement,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            // Same box as `widgets::page_column` minus its vertical padding:
            // aligns the section title with the cards the embedded page
            // renders inside its own column.
            .child(
                widgets::page_column()
                    .pt(px(0.0))
                    .pb(px(0.0))
                    .child(widgets::section_header(theme, anchor.label(), subtitle)),
            )
            .child(page)
    }
}

impl gpui::Render for GeneralPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = Theme::of(cx).clone();
        if let Some(anchor) = self.pending_anchor.take() {
            // Child 0 is the header block; sections follow in GeneralAnchor::ALL
            // order — see `GeneralAnchor::scroll_index`.
            self.scroll.scroll_to_top_of_item(anchor.scroll_index());
        }

        let chips = GeneralAnchor::ALL.iter().map(|anchor| {
            let anchor = *anchor;
            widgets::nav_chip(&theme, anchor.label(), self.active_anchor == Some(anchor))
                .id(gpui::SharedString::from(format!(
                    "general-chip-{}",
                    anchor.label()
                )))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.scroll_to(anchor, cx);
                }))
        });

        div().id("general-page").size_full().child(
            div()
                .id("general-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                // Child 0 of the scroll container: header + chips. The
                // four section wrappers below are children 1-4.
                .child(
                    widgets::page_column()
                        .pb(px(24.0))
                        .child(widgets::page_header(&theme, "General", None))
                        .child(widgets::page_subtitle(
                            &theme,
                            "Devices, security, files and local memory on this device.",
                        ))
                        .child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(8.0))
                                .children(chips),
                        ),
                )
                .child(self.section(
                    &theme,
                    GeneralAnchor::Devices,
                    "This device, paired computers and app updates.",
                    self.devices.clone().into_any_element(),
                ))
                .child(self.section(
                    &theme,
                    GeneralAnchor::Security,
                    "Default agent access for new chats.",
                    self.security.clone().into_any_element(),
                ))
                .child(self.section(
                    &theme,
                    GeneralAnchor::Files,
                    "Autosave, editor text and file browsing.",
                    self.files.clone().into_any_element(),
                ))
                .child(self.section(
                    &theme,
                    GeneralAnchor::Memory,
                    "Local activity index shared by agents on this device.",
                    self.memory.clone().into_any_element(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_cover_all_sections_in_order() {
        assert_eq!(
            GeneralAnchor::ALL,
            [
                GeneralAnchor::Devices,
                GeneralAnchor::Security,
                GeneralAnchor::Files,
                GeneralAnchor::Memory,
            ]
        );
    }

    #[test]
    fn scroll_index_skips_the_header_child() {
        assert_eq!(GeneralAnchor::Devices.scroll_index(), 1);
        assert_eq!(GeneralAnchor::Security.scroll_index(), 2);
        assert_eq!(GeneralAnchor::Files.scroll_index(), 3);
        assert_eq!(GeneralAnchor::Memory.scroll_index(), 4);
    }
}
