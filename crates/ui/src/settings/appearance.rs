//! Settings → Appearance: pick between following the system and pinning light or
//! dark.
//!
//! Uses [`widgets::option_card_row`] — a preview-card picker, because the choice
//! is a *look*, and a miniature of the result says more than a sentence about it.
//! The control itself is theme-agnostic; only the previews below know what a
//! theme is.
//!
//! The palette itself lives in the [`crate::appearance`] globals, and
//! `set_mode` repaints every window; this page only holds the in-flight state of
//! the "save this look as a theme" field.

use std::path::Path;

use gpui::{
    AnyElement, Context, Entity, Focusable, Hsla, IntoElement, PathPromptOptions, Render,
    SharedString, Subscription, Task, Window, div, prelude::*, px, relative,
};
use gpui_base::slider::{
    Slider, SliderEvent, SliderIndicator, SliderState, SliderThumb, SliderTrack,
};

use crate::appearance::{self, AppearanceMode, AppearanceState};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::icon;
use crate::settings::{AccentColor, CustomTheme, ThemePreset, UiSettings, wallpaper, widgets};
use crate::theme::Theme;

pub struct AppearancePage {
    picker_task: Option<Task<()>>,
    wallpaper_error: Option<String>,
    wallpaper_opacity_slider: Entity<SliderState>,
    _wallpaper_opacity_events: Subscription,
    /// Name field of the "save current look" flow; only shown while saving.
    theme_name: Entity<ComposerInput>,
    _theme_name_events: Subscription,
    saving_theme: bool,
    theme_error: Option<String>,
    /// Embedded mode (first-run onboarding): the controls only — the page
    /// header and subtitle belong to the host step.
    embedded: bool,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let initial_opacity = wallpaper::panel_opacity(cx) * 100.0;
        let wallpaper_opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(wallpaper::MIN_PANEL_SCRIM_ALPHA * 100.0)
                .max(wallpaper::MAX_PANEL_SCRIM_ALPHA * 100.0)
                .step(1.0)
                .default_value(initial_opacity)
        });
        let wallpaper_opacity_events = cx.subscribe(
            &wallpaper_opacity_slider,
            |this, _, event: &SliderEvent, cx| {
                let (value, persist) = match event {
                    SliderEvent::Change(value) => (value.end(), false),
                    SliderEvent::Release(value) => (value.end(), true),
                };
                if let Err(error) = wallpaper::set_panel_opacity(value / 100.0, persist, cx) {
                    tracing::warn!(error = %error, "could not save wallpaper opacity");
                    this.wallpaper_error =
                        Some(format!("Could not save wallpaper opacity: {error}"));
                } else if persist {
                    this.wallpaper_error = None;
                }
                cx.notify();
            },
        );
        let theme_name = cx.new(|cx| ComposerInput::new("Theme name", cx));
        let theme_name_events =
            cx.subscribe(&theme_name, |this, _, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.save_theme(cx);
                }
            });
        Self {
            picker_task: None,
            wallpaper_error: None,
            wallpaper_opacity_slider,
            _wallpaper_opacity_events: wallpaper_opacity_events,
            theme_name,
            _theme_name_events: theme_name_events,
            saving_theme: false,
            theme_error: None,
            embedded: false,
        }
    }

    fn begin_save_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.saving_theme = true;
        self.theme_error = None;
        self.theme_name
            .update(cx, |input, cx| input.set_text("", cx));
        window.focus(&self.theme_name.focus_handle(cx), cx);
        cx.notify();
    }

    /// Save the current accent, palette family and wallpaper as a named theme.
    /// The image is copied under `{data_dir}/themes/`, so the saved theme keeps
    /// it even after the live wallpaper is replaced or removed.
    fn save_theme(&mut self, cx: &mut Context<Self>) {
        let name = self.theme_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.theme_error = Some("Give the theme a name first.".into());
            cx.notify();
            return;
        }
        let Some(data_dir) = cx
            .try_global::<AppearanceState>()
            .map(|state| state.data_dir.clone())
        else {
            return;
        };
        let mut settings = UiSettings::load(&data_dir);
        if settings
            .custom_themes
            .iter()
            .any(|theme| theme.name.eq_ignore_ascii_case(&name))
        {
            self.theme_error = Some(format!("A theme named “{name}” already exists."));
            cx.notify();
            return;
        }
        let image = wallpaper::path(cx)
            .filter(|path| path.is_file())
            .map(|path| wallpaper::import_theme_image(&data_dir, &path))
            .transpose();
        let image = match image {
            Ok(image) => image,
            Err(error) => {
                tracing::warn!(%error, "could not copy the wallpaper into the theme");
                self.theme_error = Some(format!("Could not copy the wallpaper: {error}"));
                cx.notify();
                return;
            }
        };
        let saved = CustomTheme {
            name: name.clone(),
            accent_color: appearance::accent_color(cx),
            preset: appearance::theme_preset(cx),
            wallpaper_path: image.map(|path| path.to_string_lossy().into_owned()),
            wallpaper_scope: wallpaper::scope(cx),
        };
        settings.custom_themes.push(saved.clone());
        settings.active_custom_theme = Some(name.clone());
        if let Err(error) = settings.save(&data_dir) {
            tracing::warn!(%error, "could not save the theme");
            self.theme_error = Some(format!("Could not save the theme: {error}"));
            cx.notify();
            return;
        }
        let state = cx.global_mut::<AppearanceState>();
        state.custom_themes.push(saved);
        state.active_custom = Some(name);
        self.saving_theme = false;
        self.theme_error = None;
        self.theme_name
            .update(cx, |input, cx| input.set_text("", cx));
        cx.notify();
    }

    /// Name field + Save/Cancel for the "save this look" flow.
    fn render_theme_save_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(30.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.04))
                    .flex()
                    .items_center()
                    .child(self.theme_name.clone()),
            )
            .child(
                crate::popover::btn_primary(theme, "Save")
                    .id("theme-save-confirm")
                    .on_click(cx.listener(|this, _, _, cx| this.save_theme(cx))),
            )
            .child(
                widgets::ghost_action(theme)
                    .id("theme-save-cancel")
                    .hover(|style| widgets::ghost_hover(theme, style))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.saving_theme = false;
                        this.theme_error = None;
                        cx.notify();
                    }))
                    .child("Cancel"),
            )
            .into_any_element()
    }

    /// Embedded inside the onboarding's Appearance step.
    pub fn new_embedded(cx: &mut Context<Self>) -> Self {
        let mut page = Self::new(cx);
        page.embedded = true;
        page
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

    fn save_effect(&mut self, effect: wallpaper::BackgroundEffect, cx: &mut Context<Self>) {
        self.wallpaper_error = wallpaper::set_effect(effect, cx).err().map(|err| {
            tracing::warn!(error = %err, "could not apply background effect");
            format!("Could not apply background effect: {err}")
        });
        cx.notify();
    }

    fn render_wallpaper_opacity_control(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slider = self.wallpaper_opacity_slider.read(cx);
        let value = slider.value().end().round() as u32;
        let percentage = slider.percentage().end;
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(widgets::field_label(theme, "Panel overlay opacity"))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(format!("{value}%")),
                    ),
            )
            .child(
                Slider::new(&self.wallpaper_opacity_slider)
                    .w_full()
                    .h(px(28.0))
                    .child(
                        SliderTrack::new(&self.wallpaper_opacity_slider)
                            .relative()
                            .w_full()
                            .h_full()
                            .child(
                                div()
                                    .absolute()
                                    .top(px(13.0))
                                    .left_0()
                                    .w_full()
                                    .h(px(2.0))
                                    .rounded(px(1.0))
                                    .bg(theme.border),
                            )
                            .child(
                                SliderIndicator::new(&self.wallpaper_opacity_slider)
                                    .absolute()
                                    .top(px(13.0))
                                    .left_0()
                                    .w_full()
                                    .h(px(2.0))
                                    .child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .bottom_0()
                                            .left_0()
                                            .right(relative(1.0 - percentage))
                                            .bg(theme.accent),
                                    ),
                            )
                            .child(
                                SliderThumb::new(&self.wallpaper_opacity_slider)
                                    .absolute()
                                    .top(px(6.0))
                                    .left(relative(percentage))
                                    .ml(px(-8.0))
                                    .size(px(16.0))
                                    .rounded_full()
                                    .bg(theme.text)
                                    .border_1()
                                    .border_color(theme.border_strong),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child("Adjust how strongly the panel dims the image."),
            )
            .into_any_element()
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

/// One segmented control button. The caller adds `.id`-less click handlers.
fn segment(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(if selected {
            theme.border_strong
        } else {
            theme.border
        })
        .bg(if selected {
            crate::theme::card_selected_bg()
        } else {
            theme.surface_raised.opacity(0.5)
        })
        .text_size(px(12.5))
        .text_color(theme.text)
        .cursor_pointer()
        .child(label.into())
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

/// The wallpaper image, cropped to fill its frame.
fn cover(path: &Path) -> gpui::Img {
    gpui::img(path.to_path_buf())
        .size_full()
        .object_fit(gpui::ObjectFit::Cover)
}

/// A miniature of the app in `theme`: sidebar strip, inset content card, a few
/// placeholder lines. Built from the theme's own tokens rather than fixed
/// swatches, so the previews stay honest if the palette is retuned.
///
/// With a `wallpaper`, the image is painted where that scope puts it — behind
/// every surface for the window, inside the content card for the panel — and the
/// skeleton above it is thinned so both the image and the layout stay visible.
///
/// Rounds itself: the card frame cannot do it for us (see
/// [`widgets::OPTION_CARD_RADIUS`]). Only this root paints a background that
/// reaches the corners — the sidebar strip is transparent and the content card is
/// inset — so rounding here is enough.
fn miniature(
    theme: &Theme,
    corners: Corners,
    wallpaper: Option<(&Path, wallpaper::WallpaperScope)>,
) -> AnyElement {
    let window_image = matches!(wallpaper, Some((_, wallpaper::WallpaperScope::Window)));
    // Placeholder lines sit on a photo in the scope previews, so they need more
    // contrast than they do on a flat palette.
    let (line, strong) = if wallpaper.is_some() {
        (theme.text.opacity(0.45), theme.text.opacity(0.65))
    } else {
        (theme.text.opacity(0.22), theme.text.opacity(0.34))
    };
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div()
        .size_full()
        .relative()
        .flex()
        .flex_row()
        .bg(theme.surface);
    let root = match corners {
        Corners::All => root.rounded(r),
        Corners::Left => root.rounded_tl(r).rounded_bl(r),
        Corners::Right => root.rounded_tr(r).rounded_br(r),
    };
    let root = match wallpaper {
        Some((path, wallpaper::WallpaperScope::Window)) => root.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .child(cover(path).rounded(r)),
        ),
        _ => root,
    };
    // gpui clips children to a rectangle, not a curve, so the image and its veil
    // round themselves to the card's radius.
    let card_r = px(6.0);
    let card = div()
        // Inset content card — the same rounded plate the real shell floats.
        .relative()
        .flex_1()
        .min_w_0()
        .my(px(8.0))
        .mr(px(8.0))
        .rounded(card_r)
        .border_1()
        .border_color(theme.border)
        .bg(if window_image {
            theme.bg.opacity(0.62)
        } else {
            theme.bg
        })
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .p(px(10.0));
    let card = if let Some((path, wallpaper::WallpaperScope::Panel)) = wallpaper {
        card.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .child(cover(path).rounded(card_r)),
        )
        // The same veil the real panel lays over the image.
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .rounded(card_r)
                .bg(theme.surface_card.opacity(0.60)),
        )
    } else {
        card
    };
    root.child(
        // Sidebar strip. The window scope lets the image through the surface;
        // the panel scope keeps the chrome opaque.
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
            .bg(if window_image {
                theme.surface.opacity(0.42)
            } else {
                theme.surface
            })
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        card.child(bar(0.62, strong))
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
        .child(div().w_1_2().h_full().overflow_hidden().child(miniature(
            &Theme::light(),
            Corners::Left,
            None,
        )))
        .child(div().w_1_2().h_full().overflow_hidden().child(miniature(
            &Theme::dark(),
            Corners::Right,
            None,
        )))
        .into_any_element()
}

/// The preview graphic for a mode.
///
/// The one place `Theme::light()`/`Theme::dark()` are legitimately built outside
/// the installed global: a preview has to show the palette you are *not* using.
fn preview(mode: AppearanceMode) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(),
        AppearanceMode::Light => miniature(&Theme::light(), Corners::All, None),
        AppearanceMode::Dark => miniature(&Theme::dark(), Corners::All, None),
    }
}

/// Helper copy under the picker.
#[cfg(test)]
fn helper(mode: AppearanceMode, system: crate::theme::Appearance) -> SharedString {
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
        let current_accent = appearance::accent_color(cx);
        let current_preset = appearance::theme_preset(cx);
        let wallpaper_path = wallpaper::path(cx);
        let has_wallpaper = wallpaper_path.is_some();
        let current_scope = wallpaper::scope(cx);
        let effect = wallpaper::effect(cx);
        let opacity_block = (has_wallpaper
            && !self.embedded
            && current_scope == wallpaper::WallpaperScope::Panel
            && !effect.is_ascii())
        .then(|| self.render_wallpaper_opacity_control(&theme, cx));

        let cards = AppearanceMode::ALL.into_iter().map(|mode| {
            widgets::option_card(&theme, mode.label(), mode == current, preview(mode))
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
        });

        let active_custom = appearance::active_custom(cx);
        let mut theme_cards: Vec<AnyElement> = ThemePreset::ALL
            .into_iter()
            .map(|preset| {
                let palette = Theme::preview_for(theme.appearance, preset, current_accent);
                widgets::option_card(
                    &theme,
                    preset.label(),
                    preset == current_preset && active_custom.is_none(),
                    miniature(&palette, Corners::All, None),
                )
                .id(SharedString::from(format!(
                    "theme-preset-{}",
                    preset.label().to_lowercase().replace(' ', "-")
                )))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_theme_preset(preset, cx);
                    appearance::set_active_custom(None, cx);
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        theme_cards.extend(appearance::custom_themes(cx).into_iter().map(|saved| {
            let palette = Theme::preview_for(theme.appearance, saved.preset, saved.accent_color);
            let image = saved
                .wallpaper_path
                .as_deref()
                .map(Path::new)
                .filter(|path| path.is_file());
            let selected = active_custom.as_deref() == Some(saved.name.as_str());
            let label = saved.name.clone();
            widgets::option_card(
                &theme,
                label.clone(),
                selected,
                miniature(
                    &palette,
                    Corners::All,
                    image.map(|path| (path, saved.wallpaper_scope)),
                ),
            )
            .id(SharedString::from(format!("theme-saved-{label}")))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.theme_error = appearance::apply_custom_theme(&saved, cx)
                    .err()
                    .map(|error| {
                        tracing::warn!(%error, "could not apply a saved theme");
                        format!("Could not apply theme: {error}")
                    });
                cx.notify();
            }))
            .into_any_element()
        }));

        // Built-in and saved themes share one three-column grid.
        let mut preset_rows: Vec<gpui::Div> = Vec::new();
        while !theme_cards.is_empty() {
            let take = theme_cards.len().min(3);
            let row: Vec<AnyElement> = theme_cards.drain(..take).collect();
            preset_rows.push(
                div()
                    .flex()
                    .items_start()
                    .gap(px(16.0))
                    .children(row)
                    // A short last row keeps the three-column widths by leaving
                    // the missing slots empty instead of stretching its cards.
                    .children((take..3).map(|_| div().flex_1())),
            );
        }

        // Where the image is painted, as two miniatures of the app with the
        // chosen image already in place — the same skeleton the palette previews
        // use, so the choice is shown rather than named. Only offered once an
        // image is chosen; onboarding keeps the wallpaper step to the image
        // itself.
        let scope_row = (has_wallpaper && !self.embedded).then(|| {
            let path = wallpaper_path.as_deref().filter(|path| path.is_file());
            let cards: Vec<_> = wallpaper::WallpaperScope::ALL
                .into_iter()
                .map(|scope| {
                    widgets::option_card(
                        &theme,
                        scope.label(),
                        scope == current_scope,
                        miniature(&theme, Corners::All, path.map(|path| (path, scope))),
                    )
                    .id(SharedString::from(format!(
                        "wallpaper-scope-{}",
                        scope.label()
                    )))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.wallpaper_error = wallpaper::set_scope(scope, cx).err().map(|err| {
                            tracing::warn!(error = %err, "could not save wallpaper scope");
                            format!("Could not save wallpaper area: {err}")
                        });
                        cx.notify();
                    }))
                })
                .collect();
            let caption = match current_scope {
                wallpaper::WallpaperScope::Window => "Behind the whole window.",
                wallpaper::WallpaperScope::Panel => "Behind the main panel only.",
            };
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(widgets::option_card_row().children(cards))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(caption),
                )
        });

        // Background effect: a single choice, no knobs — picking ASCII runs the
        // fixed recipe over the image (zeron parity).
        let ascii_params = match effect {
            wallpaper::BackgroundEffect::Ascii(params) => params,
            wallpaper::BackgroundEffect::None => wallpaper::AsciiParams::default(),
        };
        let ascii_supported = wallpaper_path
            .as_deref()
            .is_some_and(wallpaper::ascii::supports_ascii);
        let effect_block = (has_wallpaper && !self.embedded).then(|| {
            let current_is_ascii = effect.is_ascii();

            let effect_buttons = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    segment(&theme, "effect-none", "None", !current_is_ascii).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.save_effect(wallpaper::BackgroundEffect::None, cx);
                        }),
                    ),
                )
                .child(
                    segment(&theme, "effect-ascii", "ASCII", current_is_ascii)
                        .when(!ascii_supported, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.save_effect(wallpaper::BackgroundEffect::Ascii(ascii_params), cx);
                        })),
                );

            div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(effect_buttons)
                .when(!ascii_supported, |el| {
                    el.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child("ASCII needs a PNG or JPEG."),
                    )
                })
        });

        let wallpaper_preview = wallpaper_path
            .as_ref()
            .filter(|path| path.is_file())
            .map(|path| match wallpaper::ascii_texture(cx) {
                Some(texture) => gpui::img(texture)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .into_any_element(),
                None => gpui::img(path.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .into_any_element(),
            });

        let page = div().id("appearance-page");
        let page = if self.embedded {
            // Inside the onboarding the host owns the scroll container and
            // sizing; the page contributes its controls only.
            page.w_full()
        } else {
            page.size_full().overflow_y_scroll()
        };
        page.child({
                let column = widgets::page_column();
                let column = if self.embedded {
                    column
                } else {
                    column
                        .child(widgets::page_header(&theme, "Appearance", None))
                        .child(
                            widgets::page_subtitle(&theme, "Choose the interface mode and palette.")
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                        )
                };
                column
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .child(widgets::field_label(&theme, "Mode"))
                            .child(widgets::option_card_row().children(cards)),
                    )
                    .child(
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Accentuation"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .children(AccentColor::ALL.into_iter().map(|color| {
                                        let selected = color == current_accent;
                                        let swatch = gpui::hsla(color.hue(), 0.78, 0.56, 1.0);
                                        div()
                                            .id(SharedString::from(format!("accent-{}", color.label().to_lowercase())))
                                            .size(px(30.0))
                                            .rounded_full()
                                            .border_2()
                                            .border_color(if selected { theme.text } else { theme.border })
                                            .bg(swatch)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .when(selected, |el| el.child(
                                                div()
                                                    .text_size(px(13.0))
                                                    .font_weight(gpui::FontWeight::BOLD)
                                                    .text_color(theme.on_accent)
                                                    .child("✓"),
                                            ))
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                appearance::set_accent_color(color, cx);
                                                appearance::set_active_custom(None, cx);
                                                cx.notify();
                                            }))
                                    })),
                            )
                    )
                    .child(
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Wallpaper"))
                            .child(div()
                                .flex()
                                .flex_col()
                                .gap(px(14.0))
                                .rounded(px(14.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.surface.opacity(0.42))
                                .p(px(14.0))
                                .child(div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(14.0))
                                    .child(div()
                                        .flex()
                                        .items_center()
                                        .gap(px(12.0))
                                        .child(div()
                                            .size(px(68.0))
                                            .rounded(px(10.0))
                                            .border_1()
                                            .border_color(theme.border)
                                            .overflow_hidden()
                                            .bg(theme.surface_raised)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(wallpaper_preview.unwrap_or_else(|| icon(crate::icons::IMAGE)
                                                .size(px(24.0)).text_color(theme.text_muted).into_any_element())))
                                        .child(div().flex().flex_col().gap(px(4.0))
                                            .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text).child(if has_wallpaper { "Custom wallpaper" } else { "No wallpaper selected" }))
                                            .child(div().text_size(px(12.0)).text_color(theme.text_muted)
                                                .child("Choose an image to personalize the workspace."))))
                                    .child(div().flex().items_center().gap(px(8.0))
                                        .child(widgets::ghost_action(&theme).id("wallpaper-choose")
                                            .on_click(cx.listener(|this, _, _, cx| this.open_wallpaper_picker(cx)))
                                            .child(icon(crate::icons::IMAGE).size(px(16.0)).text_color(theme.text_muted))
                                            .child(if has_wallpaper { "Change" } else { "Choose image" }))
                                        .when(has_wallpaper, |el| el.child(widgets::ghost_action(&theme)
                                            .id("wallpaper-remove")
                                            .on_click(cx.listener(|this, _, _, cx| this.remove_wallpaper(cx)))
                                            .child(icon(crate::icons::TRASH_BIN_MINIMALISTIC).size(px(16.0)).text_color(theme.text_muted))
                                            .child("Remove")))))
                                .children(scope_row)
                                .children(opacity_block)
                                .children(effect_block)
                                .when_some(self.wallpaper_error.clone(), |el, error| el.child(
                                    div().mt(px(2.0)).text_size(px(12.0)).text_color(theme.danger_muted).child(error))))
                            ,
                    )
                    .child(
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(widgets::field_label(&theme, "Theme"))
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id("theme-save-as")
                                            .hover(|style| widgets::ghost_hover(&theme, style))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.begin_save_theme(window, cx)
                                            }))
                                            .child(
                                                icon(crate::icons::PLUS)
                                                    .size(px(14.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child("Save current"),
                                    ),
                            )
                            .when(self.saving_theme, |el| {
                                el.child(self.render_theme_save_row(&theme, cx))
                            })
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child("Komet palettes and your saved looks, designed to work with the mode and accent above."),
                            )
                            .children(preset_rows)
                            .when_some(self.theme_error.clone(), |el, error| {
                                el.child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme.danger_muted)
                                        .child(error),
                                )
                            }),
                    )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Appearance;

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

    /// The preset grid is only useful if the palettes actually differ. The
    /// previews are built from these tones, so comparing them is the closest we
    /// get without a renderer.
    #[test]
    fn theme_presets_draw_from_distinct_palettes() {
        for (index, a) in ThemePreset::ALL.iter().enumerate() {
            for b in &ThemePreset::ALL[index + 1..] {
                for appearance in [Appearance::Dark, Appearance::Light] {
                    let (a, b) = (
                        Theme::preview_for(appearance, *a, AccentColor::Indigo),
                        Theme::preview_for(appearance, *b, AccentColor::Indigo),
                    );
                    let distinct = (a.bg.h - b.bg.h).abs() > 0.01
                        || (a.bg.s - b.bg.s).abs() > 0.005
                        || (a.bg.l - b.bg.l).abs() > 0.005;
                    assert!(distinct, "two presets share a palette in {appearance:?}");
                }
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
