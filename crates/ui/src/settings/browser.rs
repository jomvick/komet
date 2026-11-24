//! Settings → Browser : moteur de recherche, page d'accueil, historique,
//! zoom, dossier des captures, émulation et effacement des données.
//!
//! La page travaille sur une copie des [`BrowserPrefs`] et émet
//! [`BrowserSettingsEvent::Changed`] à chaque retouche — le shell persiste
//! dans `ui-settings.json` (pattern `NotificationsPage`). Les actions
//! « Clear… » sont relayées au shell qui vide les fichiers de stockage.

use gpui::{Context, EventEmitter, SharedString, Subscription, Window, div, prelude::*, px};

use crate::browser::EMULATION_PRESETS;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::settings::BrowserPrefs;
use crate::settings::widgets;
use crate::theme::{Theme, ink};

/// Événements de la page : `Changed` = une préférence a bougé (persistée par
/// le shell), `ClearHistory` / `ClearBrowsingData` = vider les fichiers
/// d'historique (+ données de navigation pour la PR Clear Storage).
#[derive(Debug)]
pub enum BrowserSettingsEvent {
    Changed(BrowserPrefs),
    ClearHistory,
    ClearBrowsingData,
}

/// Moteurs de recherche proposés : le modèle `{q}` est remplacé par la
/// requête encodée. Toute autre valeur = « Custom » (champ libre).
const SEARCH_PRESETS: &[(&str, &str)] = &[
    ("DuckDuckGo", "https://duckduckgo.com/?q={q}"),
    ("Google", "https://www.google.com/search?q={q}"),
    ("Brave", "https://search.brave.com/search?q={q}"),
    ("Bing", "https://www.bing.com/search?q={q}"),
];

/// Nom du preset correspondant au modèle stocké ; `None` = « Custom ».
pub fn search_preset(template: &str) -> Option<&'static str> {
    SEARCH_PRESETS
        .iter()
        .find(|(_, t)| *t == template)
        .map(|(name, _)| *name)
}

/// Paliers de zoom proposés (sous-ensemble des `ZOOM_LEVELS` du navigateur).
const ZOOM_OPTIONS: &[(f32, &str)] = &[
    (0.5, "50%"),
    (0.75, "75%"),
    (1.0, "100%"),
    (1.25, "125%"),
    (1.5, "150%"),
];

/// Rétention de l'historique : `None` = illimité.
const RETENTION_OPTIONS: &[(Option<u32>, &str)] = &[
    (Some(30), "30 days"),
    (Some(90), "90 days"),
    (Some(365), "1 year"),
    (None, "Forever"),
];

pub struct BrowserPage {
    prefs: BrowserPrefs,
    home_input: gpui::Entity<ComposerInput>,
    search_input: gpui::Entity<ComposerInput>,
    capture_input: gpui::Entity<ComposerInput>,
    download_input: gpui::Entity<ComposerInput>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<BrowserSettingsEvent> for BrowserPage {}

impl BrowserPage {
    pub fn new(prefs: BrowserPrefs, cx: &mut Context<Self>) -> Self {
        let home_input = cx.new(|cx| ComposerInput::new("https://example.com", cx));
        home_input.update(cx, |input, cx| {
            input.set_text(prefs.home_url.clone().unwrap_or_default(), cx);
        });
        let search_input = cx.new(|cx| ComposerInput::new("https://search.example/?q={q}", cx));
        // Le champ « Custom » n'affiche le modèle que s'il n'est pas un preset.
        let custom = search_preset(&prefs.search_url).is_none();
        search_input.update(cx, |input, cx| {
            input.set_text(
                if custom {
                    prefs.search_url.clone()
                } else {
                    String::new()
                },
                cx,
            );
        });
        let capture_input = cx.new(|cx| ComposerInput::new("~/Downloads", cx));
        capture_input.update(cx, |input, cx| {
            input.set_text(prefs.capture_dir.clone().unwrap_or_default(), cx);
        });

        let mut subs = Vec::new();
        subs.push(cx.subscribe(
            &home_input,
            |this: &mut Self,
             input: gpui::Entity<ComposerInput>,
             event: &ComposerInputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, ComposerInputEvent::Edited) {
                    this.prefs.home_url = opt_text(&input, cx);
                    this.emit(cx);
                }
            },
        ));
        subs.push(cx.subscribe(
            &search_input,
            |this: &mut Self,
             input: gpui::Entity<ComposerInput>,
             event: &ComposerInputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, ComposerInputEvent::Edited) {
                    this.prefs.search_url = input.read(cx).text().trim().to_string();
                    this.emit(cx);
                }
            },
        ));
        subs.push(cx.subscribe(
            &capture_input,
            |this: &mut Self,
             input: gpui::Entity<ComposerInput>,
             event: &ComposerInputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, ComposerInputEvent::Edited) {
                    this.prefs.capture_dir = opt_text(&input, cx);
                    this.emit(cx);
                }
            },
        ));

        let download_input = cx.new(|cx| ComposerInput::new("~/Downloads", cx));
        download_input.update(cx, |input, cx| {
            input.set_text(prefs.download_dir.clone().unwrap_or_default(), cx);
        });
        subs.push(cx.subscribe(
            &download_input,
            |this: &mut Self,
             input: gpui::Entity<ComposerInput>,
             event: &ComposerInputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, ComposerInputEvent::Edited) {
                    this.prefs.download_dir = opt_text(&input, cx);
                    this.emit(cx);
                }
            },
        ));

        Self {
            prefs,
            home_input,
            search_input,
            capture_input,
            download_input,
            _subs: subs,
        }
    }

    fn emit(&mut self, cx: &mut Context<Self>) {
        cx.emit(BrowserSettingsEvent::Changed(self.prefs.clone()));
        cx.notify();
    }

    fn set_search_preset(&mut self, name: &str, cx: &mut Context<Self>) {
        if name == "Custom" {
            // Modèle vide que l'utilisateur complète dans le champ.
            if search_preset(&self.prefs.search_url).is_some() {
                self.prefs.search_url = String::new();
                self.search_input
                    .update(cx, |input, cx| input.set_text("", cx));
            }
        } else if let Some((_, template)) = SEARCH_PRESETS.iter().find(|(n, _)| *n == name) {
            self.prefs.search_url = (*template).to_string();
            self.search_input
                .update(cx, |input, cx| input.set_text("", cx));
        }
        self.emit(cx);
    }

    /// Rangée de pastilles exclusives (moteur de recherche, rétention, zoom,
    /// émulation). `options` = (libellé, valeur persistée).
    fn chip_row<V: PartialEq + Clone + 'static>(
        &self,
        theme: &Theme,
        options: Vec<(SharedString, V)>,
        current: &V,
        row_id: &'static str,
        on_pick: impl Fn(&mut Self, V, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let on_pick = std::rc::Rc::new(on_pick);
        let mut row = div().flex().flex_row().flex_wrap().gap(px(6.0));
        for (ix, (label, value)) in options.into_iter().enumerate() {
            let selected = *current == value;
            let on_pick = on_pick.clone();
            row = row.child(
                div()
                    .id((row_id, ix))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded_full()
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .bg(if selected {
                        theme.accent.opacity(0.14)
                    } else {
                        ink(0.04)
                    })
                    .text_size(px(12.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(ink(0.08)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(this, value.clone(), cx);
                    })),
            );
        }
        row
    }

    /// Champ texte bordé contenant un `ComposerInput`.
    fn text_field(&self, theme: &Theme, input: gpui::Entity<ComposerInput>) -> gpui::Div {
        div()
            .h(px(30.0))
            .w_full()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(ink(0.04))
            .px(px(8.0))
            .flex()
            .items_center()
            .child(input)
    }

    /// Ligne « titre + méta + contenu » dans une carte (icône incluse).
    fn field_row(
        &self,
        theme: &Theme,
        icon: &'static str,
        title: &str,
        meta: &str,
        body: gpui::AnyElement,
    ) -> gpui::Div {
        widgets::card_row(theme, false)
            .child(widgets::row_tile(theme, icon))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, title))
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            div()
                                .child(SharedString::from(meta.to_string()))
                                .into_any_element(),
                        ],
                    ))
                    .child(div().mt(px(6.0)).child(body)),
            )
    }
}

/// `Some(texte)` si non vide après trim, `None` sinon.
fn opt_text(input: &gpui::Entity<ComposerInput>, cx: &gpui::App) -> Option<String> {
    let text = input.read(cx).text().trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl Render for BrowserPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let prefs = self.prefs.clone();

        // --- Search & home -------------------------------------------------
        let preset_name = search_preset(&prefs.search_url).unwrap_or("Custom");
        let is_custom = preset_name == "Custom";
        let mut preset_options: Vec<(SharedString, String)> = SEARCH_PRESETS
            .iter()
            .map(|(name, _)| (SharedString::from(*name), (*name).to_string()))
            .collect();
        preset_options.push((SharedString::from("Custom"), "Custom".to_string()));

        let search_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::SEARCH))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Search engine"))
                            .child(
                                self.chip_row(
                                    &theme,
                                    preset_options,
                                    &preset_name.to_string(),
                                    "browser-search-preset",
                                    |this, value, cx| this.set_search_preset(&value, cx),
                                    cx,
                                )
                                .mt(px(8.0)),
                            )
                            .when(is_custom, |el| {
                                el.child(
                                    div()
                                        .mt(px(8.0))
                                        .child(self.text_field(&theme, self.search_input.clone())),
                                )
                            }),
                    ),
            )
            .child(
                self.field_row(
                    &theme,
                    icons::HOME,
                    "Home page",
                    "URL opened by new tabs. Empty = blank page.",
                    self.text_field(&theme, self.home_input.clone())
                        .into_any_element(),
                ),
            );

        // --- History -------------------------------------------------------
        let history_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::ARCHIVE_MINIMALISTIC))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Browsing history"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Record visited pages for the address bar, panel and empty state.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        div()
                            .id("browser-history-toggle")
                            .cursor_pointer()
                            .child(widgets::toggle_switch(&theme, prefs.history_enabled))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.prefs.history_enabled = !this.prefs.history_enabled;
                                this.emit(cx);
                            })),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::CLOCK))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Keep history for"))
                            .child(
                                self.chip_row(
                                    &theme,
                                    RETENTION_OPTIONS
                                        .iter()
                                        .map(|(days, label)| {
                                            (SharedString::from(*label), *days)
                                        })
                                        .collect(),
                                    &prefs.history_retention_days,
                                    "browser-retention",
                                    |this, value, cx| {
                                        this.prefs.history_retention_days = value;
                                        this.emit(cx);
                                    },
                                    cx,
                                )
                                .mt(px(8.0)),
                            ),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::TRASH_BIN_MINIMALISTIC))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Clear history…"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Removes history and favorites from this device.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("browser-clear-history")
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .child(SharedString::from("Clear"))
                            .on_click(cx.listener(|_this, _, _, cx| {
                                cx.emit(BrowserSettingsEvent::ClearHistory);
                            })),
                    ),
            );

        // --- Zoom ----------------------------------------------------------
        let zoom_current = prefs.zoom_default;
        let mut zoom_options: Vec<(SharedString, f32)> = ZOOM_OPTIONS
            .iter()
            .map(|(level, label)| (SharedString::from(*label), *level))
            .collect();
        // Un niveau hors des 5 choix usuels (réglé ailleurs) garde sa pastille.
        if !zoom_options
            .iter()
            .any(|(_, v)| (*v - zoom_current).abs() < 0.001)
        {
            zoom_options.push((
                SharedString::from(format!("{}%", (zoom_current * 100.0).round() as i32)),
                zoom_current,
            ));
        }
        let zoom_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::ZOOM_IN))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Default zoom"))
                            .child(
                                self.chip_row(
                                    &theme,
                                    zoom_options,
                                    &zoom_current,
                                    "browser-zoom",
                                    |this, value, cx| {
                                        this.prefs.zoom_default = value;
                                        this.emit(cx);
                                    },
                                    cx,
                                )
                                .mt(px(8.0)),
                            ),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::GLOBAL))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Remember zoom per site"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Ctrl+= / Ctrl+- / Ctrl+0 adjusts zoom; each host keeps its own level.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        div()
                            .id("browser-remember-zoom")
                            .cursor_pointer()
                            .child(widgets::toggle_switch(&theme, prefs.remember_zoom))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.prefs.remember_zoom = !this.prefs.remember_zoom;
                                this.emit(cx);
                            })),
                    ),
            );

        // --- Captures + téléchargements -------------------------------------
        let capture_card = widgets::section_card(&theme)
            .child(
                self.field_row(
                    &theme,
                    icons::CAMERA,
                    "Capture folder",
                    "Screenshots from the DevTools drawer are saved here (default: Downloads).",
                    self.text_field(&theme, self.capture_input.clone())
                        .into_any_element(),
                ),
            )
            .child(
                self.field_row(
                    &theme,
                    icons::ALT_ARROW_DOWN,
                    "Downloads folder",
                    "Where page downloads land; Open/Reveal from the Downloads panel (default: Downloads).",
                    self.text_field(&theme, self.download_input.clone())
                        .into_any_element(),
                ),
            );

        // --- Emulation -----------------------------------------------------
        let emu_current = EMULATION_PRESETS
            .iter()
            .find(|(slug, _)| Some(*slug) == prefs.emulation_preset.as_deref())
            .map(|(slug, _)| *slug)
            .unwrap_or("off");
        let emulation_card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::SMARTPHONE))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Device emulation"))
                        .child(
                            self.chip_row(
                                &theme,
                                EMULATION_PRESETS
                                    .iter()
                                    .map(|(slug, label)| (SharedString::from(*label), *slug))
                                    .collect(),
                                &emu_current,
                                "browser-emulation",
                                |this, value, cx| {
                                    this.prefs.emulation_preset =
                                        (value != "off").then(|| value.to_string());
                                    this.emit(cx);
                                },
                                cx,
                            )
                            .mt(px(8.0)),
                        ),
                ),
        );

        // --- Clear browsing data -------------------------------------------
        let privacy_card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::TRASH_BIN_MINIMALISTIC))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Clear browsing data…"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![div()
                                .child(SharedString::from(
                                    "Cookies, cache, site data and history — asks for confirmation first.",
                                ))
                                .into_any_element()],
                        )),
                )
                .child(
                    widgets::ghost_action(&theme)
                        .id("browser-clear-data")
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .child(SharedString::from("Clear…"))
                        .on_click(cx.listener(|_this, _, _, cx| {
                            cx.emit(BrowserSettingsEvent::ClearBrowsingData);
                        })),
                ),
        );

        div()
            .id("browser-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Browser", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Search engine, history, zoom and capture settings for the \
                             built-in browser.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(search_card)
                    .child(history_card)
                    .child(zoom_card)
                    .child(capture_card)
                    .child(emulation_card)
                    .child(privacy_card),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_preset_recognizes_known_templates() {
        assert_eq!(
            search_preset("https://duckduckgo.com/?q={q}"),
            Some("DuckDuckGo")
        );
        assert_eq!(
            search_preset("https://www.bing.com/search?q={q}"),
            Some("Bing")
        );
        assert_eq!(search_preset("https://searx.example/?q={q}"), None);
        assert_eq!(search_preset(""), None);
    }

    #[test]
    fn emulation_presets_have_unique_slugs_with_off_first() {
        assert_eq!(EMULATION_PRESETS[0].0, "off");
        let mut slugs: Vec<&str> = EMULATION_PRESETS.iter().map(|(s, _)| *s).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), EMULATION_PRESETS.len());
    }
}
