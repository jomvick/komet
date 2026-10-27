//! Settings → Appearance: pick between following the system and pinning light or
//! dark.
//!
//! Uses [`widgets::option_card_row`] — a preview-card picker, because the choice
//! is a *look*, and a miniature of the result says more than a sentence about it.
//! The control itself is theme-agnostic; only the previews below know what a
//! theme is.
//!
//! Stateless. The choice lives in the [`crate::appearance`] globals, and
//! `set_mode` repaints every window, so this page has nothing of its own to hold.

use std::path::Path;

use gpui::{
    AnyElement, Context, Hsla, IntoElement, PathPromptOptions, Render, SharedString, Task, Window,
    div, prelude::*, px,
};

use crate::appearance::{self, AppearanceMode};
use crate::icons::icon;
use crate::settings::{wallpaper, widgets};
use crate::theme::{Appearance, Theme};

pub struct AppearancePage {
    picker_task: Option<Task<()>>,
    wallpaper_error: Option<String>,
}

impl AppearancePage {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            picker_task: None,
            wallpaper_error: None,
        }
    }

    fn open_wallpaper_picker(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose wallpaper".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let error = if !is_image_file(&path) {
                    Some("Choose a supported image file.".to_string())
                } else {
                    None
                };
                if let Err(err) = this.update(cx, |page, cx| {
                    if let Some(error) = error {
                        page.wallpaper_error = Some(error);
                    } else {
                        page.wallpaper_error =
                            wallpaper::set_path(Some(path), cx).err().map(|err| {
                                tracing::warn!(error = %err, "could not save wallpaper");
                                format!("Could not save wallpaper: {err}")
                            });
                    }
                    cx.notify();
                }) {
                    tracing::debug!(error = %err, "appearance page closed while picking wallpaper");
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => {
                report_picker_error(this, cx, format!("Could not open image picker: {err}"));
            }
            Err(err) => {
                report_picker_error(this, cx, format!("Image picker was interrupted: {err}"));
            }
        }));
    }

    fn remove_wallpaper(&mut self, cx: &mut Context<Self>) {
        self.wallpaper_error = wallpaper::set_path(None, cx).err().map(|err| {
            tracing::warn!(error = %err, "could not remove wallpaper");
            format!("Could not remove wallpaper: {err}")
        });
        cx.notify();
    }
}

fn report_picker_error(
    page: gpui::WeakEntity<AppearancePage>,
    cx: &mut gpui::AsyncApp,
    message: String,
) {
    tracing::warn!(error = %message, "wallpaper image picker failed");
    if let Err(err) = page.update(cx, |page, cx| {
        page.wallpaper_error = Some(message);
        cx.notify();
    }) {
        tracing::debug!(error = %err, "appearance page closed while reporting wallpaper picker error");
    }
}

fn is_image_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                gpui::Img::extensions()
                    .iter()
                    .any(|supported| supported.eq_ignore_ascii_case(extension))
            })
}

/// One placeholder bar in the miniature, width given as a fraction of its
/// container.
///
/// Relative rather than fixed px because the System card renders this same
/// miniature into *half* a card. Fixed widths were wider than the squeezed
/// content pane and spilled out over the card edge.
fn bar(fraction: f32, tone: Hsla) -> gpui::Div {
    div()
        .h(px(5.0))
        .w(gpui::relative(fraction))
        .rounded(px(3.0))
        .bg(tone)
}

/// Which corners a miniature rounds — the split card needs each half to round
/// only its outer side so the two meet flush down the middle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corners {
    All,
    Left,
    Right,
}

/// A miniature of the app in `theme`: sidebar strip, inset content card, a few
/// placeholder lines. Built from the theme's own tokens rather than fixed
/// swatches, so the previews stay honest if the palette is retuned.
///
/// Rounds itself: the card frame cannot do it for us (see
/// [`widgets::OPTION_CARD_RADIUS`]). Only this root paints a background that
/// reaches the corners — the sidebar strip is transparent and the content card is
/// inset — so rounding here is enough.
fn miniature(theme: &Theme, corners: Corners) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div().size_full().flex().flex_row().bg(theme.surface);
    let root = match corners {
        Corners::All => root.rounded(r),
        Corners::Left => root.rounded_tl(r).rounded_bl(r),
        Corners::Right => root.rounded_tr(r).rounded_br(r),
    };
    root.child(
        // Sidebar strip.
        div()
            .w(px(44.0))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(8.0))
            .pt(px(14.0))
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        // Inset content card — the same rounded plate the real shell floats.
        div()
            .flex_1()
            .min_w_0()
            .my(px(8.0))
            .mr(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(10.0))
            .child(bar(0.62, strong))
            .child(bar(0.88, line))
            .child(bar(0.76, line))
            .child(bar(0.52, line)),
    )
    .into_any_element()
}

/// The System card: light on the left, dark on the right. Each half is a
/// complete miniature clipped to its side, which is what makes the card read as
/// "whichever one the system is on".
fn miniature_split() -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::light(), Corners::Left)),
        )
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::dark(), Corners::Right)),
        )
        .into_any_element()
}

/// The preview graphic for a mode.
///
/// The one place `Theme::light()`/`Theme::dark()` are legitimately built outside
/// the installed global: a preview has to show the palette you are *not* using.
fn preview(mode: AppearanceMode) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(),
        AppearanceMode::Light => miniature(&Theme::light(), Corners::All),
        AppearanceMode::Dark => miniature(&Theme::dark(), Corners::All),
    }
}

/// Helper copy under the picker.
fn helper(mode: AppearanceMode, system: Appearance) -> SharedString {
    match mode {
        // Naming the resolved appearance makes "System" concrete — otherwise the
        // card says nothing about what you actually get right now.
        AppearanceMode::System => {
            let resolved = if system.is_dark() { "dark" } else { "light" };
            format!(
                "Following the system appearance — currently {resolved}. Komet switches with \
                 macOS, including scheduled changes."
            )
            .into()
        }
        AppearanceMode::Light => "Always light, whatever the system is set to.".into(),
        AppearanceMode::Dark => "Always dark, whatever the system is set to.".into(),
    }
}

impl Render for AppearancePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let current = appearance::mode(cx);
        let system = cx
            .try_global::<appearance::AppearanceState>()
            .map(|state| state.system)
            .unwrap_or_default();
        let wallpaper_path = wallpaper::path(cx);
        let has_wallpaper = wallpaper_path.is_some();

        let cards = AppearanceMode::ALL.into_iter().map(|mode| {
            widgets::option_card(&theme, mode.label(), mode == current, preview(mode))
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
        });

        let wallpaper_preview = wallpaper_path
            .as_ref()
            .filter(|path| path.is_file())
            .map(|path| {
                gpui::img(path.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .into_any_element()
            });

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Appearance", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose a theme for this device.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Theme"))
                            .child(widgets::option_card_row().children(cards)),
                    )
                    .child(
                        div()
                            .mt(px(20.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(widgets::field_label(&theme, "Wallpaper"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(12.0))
                                    .rounded(px(12.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.surface.opacity(0.52))
                                    .p(px(12.0))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(10.0))
                                            .text_size(px(13.0))
                                            .text_color(theme.text)
                                            .child(
                                                div()
                                                    .size(px(54.0))
                                                    .rounded(px(10.0))
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .overflow_hidden()
                                                    .bg(theme.surface)
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        wallpaper_preview
                                                            .unwrap_or_else(|| {
                                                                icon(crate::icons::IMAGE)
                                                                    .size(px(24.0))
                                                                    .text_color(theme.text_muted)
                                                                    .into_any_element()
                                                            }),
                                                    ),
                                            )
                                            .child(div().child("No image")),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                div()
                                                    .px(px(12.0))
                                                    .py(px(8.0))
                                                    .rounded(px(10.0))
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .bg(theme.surface_raised.opacity(0.8))
                                                    .text_size(px(12.5))
                                                    .text_color(theme.text)
                                                    .child("Image"),
                                            ),
                                    ),
                            )
                            .child(
                                if has_wallpaper {
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id("wallpaper-choose")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.open_wallpaper_picker(cx);
                                                }))
                                                .child(
                                                    icon(crate::icons::IMAGE)
                                                        .size(px(16.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Choose image"),
                                        )
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id("wallpaper-remove")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.remove_wallpaper(cx);
                                                }))
                                                .child(
                                                    icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                                        .size(px(16.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Remove"),
                                        )
                                } else {
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id("wallpaper-choose")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.open_wallpaper_picker(cx);
                                                }))
                                                .child(
                                                    icon(crate::icons::IMAGE)
                                                        .size(px(16.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Choose image"),
                                        )
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id("wallpaper-link")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.wallpaper_error = Some(
                                                        "Remote wallpaper URLs are not yet supported in this build."
                                                            .to_string(),
                                                    );
                                                    cx.notify();
                                                }))
                                                .child(
                                                    icon(crate::icons::GLOBAL)
                                                        .size(px(16.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("From a link"),
                                        )
                                }
                            )
                            .when_some(self.wallpaper_error.clone(), |el, error| {
                                el.child(
                                    div()
                                        .mt(px(4.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.danger_muted)
                                        .child(error),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .line_height(px(18.0))
                            .child(helper(current, system)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_card() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn system_helper_names_the_resolved_appearance() {
        let dark = helper(AppearanceMode::System, Appearance::Dark);
        let light = helper(AppearanceMode::System, Appearance::Light);
        assert!(dark.contains("currently dark"), "got {dark}");
        assert!(light.contains("currently light"), "got {light}");
    }

    /// The pinned modes must not claim to follow anything — that copy is the only
    /// thing telling the user the system setting is being ignored.
    #[test]
    fn pinned_helpers_do_not_mention_following() {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            for system in [Appearance::Light, Appearance::Dark] {
                let copy = helper(mode, system).to_lowercase();
                assert!(!copy.contains("following"), "{mode:?}: {copy}");
                assert!(copy.contains("whatever the system"), "{mode:?}: {copy}");
            }
        }
    }

    /// The previews must differ from each other, or the picker is decoration.
    /// Comparing the tones they are built from is the closest we can get without
    /// a renderer.
    #[test]
    fn light_and_dark_previews_draw_from_different_palettes() {
        let (l, d) = (Theme::light(), Theme::dark());
        assert_ne!(l.surface.l, d.surface.l);
        assert_ne!(l.bg.l, d.bg.l);
    }
}
