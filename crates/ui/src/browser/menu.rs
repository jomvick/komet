//! Données pures du menu « … » et des raccourcis du navigateur : états
//! grisés, items et mapping touche → action, testables sans GPUI.
//!
//! Le menu grandit PR par PR : [`action_shipped`] filtre ce qui est rendu —
//! une entrée non livrée n'apparaît pas plutôt que d'afficher un no-op.

use gpui::{Keystroke, SharedString};

use super::{ZOOM_DEFAULT, ZOOM_MAX, ZOOM_MIN};

/// Action déclenchée par une entrée du menu « … », un raccourci clavier ou
/// la barre d'outils. Les variantes sans implémentation arrivent avec leur PR
/// (zoom, find, historique…) — la surface les dispatche dans
/// `BrowserSurface::run_browser_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserAction {
    NewTab,
    CopyLink,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    FindInPage,
    DeviceEmulation,
    OpenExternalBrowser,
    History,
    ToggleFavorite,
    ClearStorage,
    BrowserSettings,
    ClearHistory,
    Screenshot,
    ToggleConsole,
}

/// Actions câblées à ce jour. PR 1 : navigation, navigateur externe,
/// settings. PR 2 : l'élément (icône dédiée) et le tiroir DevTools —
/// Screenshot et Console ont quitté le menu pour l'en-tête du tiroir.
/// PR 3 : History et Favorites.
/// PR 8 : Clear Storage (dialogue de confirmation).
pub fn action_shipped(action: BrowserAction) -> bool {
    matches!(
        action,
        BrowserAction::NewTab
            | BrowserAction::CopyLink
            | BrowserAction::OpenExternalBrowser
            | BrowserAction::BrowserSettings
            | BrowserAction::History
            | BrowserAction::ToggleFavorite
            // PR 5 : paliers de zoom + viewport effectif.
            | BrowserAction::ZoomIn
            | BrowserAction::ZoomOut
            | BrowserAction::ResetZoom
            // PR 6 : barre de recherche flottante (CSS Custom Highlight).
            | BrowserAction::FindInPage
            // PR 7 : barre d'émulation device + cadre centré.
            | BrowserAction::DeviceEmulation
            // PR 8 : dialogue Clear Storage (cookies/cache/site data/history).
            | BrowserAction::ClearStorage
            | BrowserAction::ClearHistory
    )
}

/// États « grisé » du menu, calculés sans UI : `has_page` = image affichée
/// et URL http(s) ; `zoom` = niveau courant (1.0 = 100 %) ; `favorite` sert
/// aux libellés (Add/Remove), pas aux états.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuStates {
    pub new_tab: bool,
    pub copy_link: bool,
    pub zoom_in: bool,
    pub zoom_out: bool,
    pub reset_zoom: bool,
    pub find_in_page: bool,
    pub device_emulation: bool,
    pub open_external: bool,
    pub history: bool,
    pub toggle_favorite: bool,
    pub clear_storage: bool,
    pub clear_history: bool,
    pub browser_settings: bool,
}

const ZOOM_EPS: f32 = 0.001;

pub fn menu_state(has_page: bool, zoom: f32, _favorite: bool) -> MenuStates {
    MenuStates {
        new_tab: true,
        copy_link: has_page,
        zoom_in: has_page && zoom < ZOOM_MAX - ZOOM_EPS,
        zoom_out: has_page && zoom > ZOOM_MIN + ZOOM_EPS,
        reset_zoom: has_page && (zoom - ZOOM_DEFAULT).abs() > ZOOM_EPS,
        find_in_page: has_page,
        // Device emulation, history, clear storage, settings : actifs même
        // sans page (la capture les montre cliquables à l'état vide).
        device_emulation: true,
        open_external: has_page,
        history: true,
        toggle_favorite: has_page,
        clear_storage: true,
        clear_history: true,
        browser_settings: true,
    }
}

/// Une ligne du menu « … » : libellé, raccourci affiché, action, état.
pub struct MenuItem {
    pub label: SharedString,
    pub shortcut: Option<SharedString>,
    pub action: BrowserAction,
    pub enabled: bool,
    /// `true` = séparateur fin au-dessus de la ligne (limite de groupe).
    pub separated: bool,
}

/// Libellé du raccourci : `Ctrl+T`, `⌘T` sur macOS (comme l'état vide).
fn shortcut_label(key: &str) -> SharedString {
    if cfg!(target_os = "macos") {
        format!("⌘{key}").into()
    } else {
        format!("Ctrl+{key}").into()
    }
}

/// Les entrées de la capture, dans l'ordre des groupes. `favorite` ne change
/// que le libellé (« Remove from Favorites »).
/// Le menu « … » réduit à six entrées (capture utilisateur) : zoom, copier le
/// lien, ouvrir dans le navigateur, effacer l'historique. Les autres actions
/// restent disponibles par raccourci ou dans les réglages.
pub fn all_menu_items(states: &MenuStates, _favorite: bool) -> Vec<MenuItem> {
    vec![
        MenuItem {
            label: "Zoom in".into(),
            shortcut: Some(shortcut_label("+")),
            action: BrowserAction::ZoomIn,
            enabled: states.zoom_in,
            separated: false,
        },
        MenuItem {
            label: "Zoom out".into(),
            shortcut: Some(shortcut_label("-")),
            action: BrowserAction::ZoomOut,
            enabled: states.zoom_out,
            separated: false,
        },
        MenuItem {
            label: "Reset zoom".into(),
            shortcut: None,
            action: BrowserAction::ResetZoom,
            enabled: states.reset_zoom,
            separated: false,
        },
        MenuItem {
            label: "Copy link".into(),
            shortcut: None,
            action: BrowserAction::CopyLink,
            enabled: states.copy_link,
            separated: true,
        },
        MenuItem {
            label: "Open in browser".into(),
            shortcut: None,
            action: BrowserAction::OpenExternalBrowser,
            enabled: states.open_external,
            separated: false,
        },
        MenuItem {
            label: "Clear history".into(),
            shortcut: None,
            action: BrowserAction::ClearHistory,
            enabled: states.clear_history,
            separated: true,
        },
    ]
}

/// Raccourci clavier → action. Appelée dans `on_page_key_down` AVANT le
/// forwarding vers Chrome : sans ça Ctrl+F ou Ctrl+T partent dans la page.
/// `mod` = Ctrl ou Cmd selon la plateforme (gpui `control`/`platform`).
pub fn browser_shortcut(keystroke: &Keystroke) -> Option<BrowserAction> {
    let modifiers = &keystroke.modifiers;
    let mod_pressed = modifiers.control || modifiers.platform;
    let key = keystroke.key.as_str();

    // DevTools : F12 seul, ou Ctrl/Cmd+Shift+I.
    if key == "f12" && !modifiers.alt && !modifiers.shift {
        return Some(BrowserAction::ToggleConsole);
    }
    if key == "i" && mod_pressed && modifiers.shift && !modifiers.alt {
        return Some(BrowserAction::ToggleConsole);
    }
    if !mod_pressed || modifiers.alt {
        return None;
    }
    match key {
        "t" if !modifiers.shift => Some(BrowserAction::NewTab),
        // « + » couvre Ctrl+Shift+= (le key_char reste « + » sur certains
        // layouts) ; « _ » couvre Ctrl+Shift+-.
        "=" | "+" => Some(BrowserAction::ZoomIn),
        "-" | "_" => Some(BrowserAction::ZoomOut),
        // NumPad0 : gpui peut le livrer sous différents noms selon la
        // plateforme — on accepte les variantes connues.
        "0" | "num_pad_0" | "numpad0" => Some(BrowserAction::ResetZoom),
        "f" if !modifiers.shift => Some(BrowserAction::FindInPage),
        "h" if !modifiers.shift => Some(BrowserAction::History),
        "d" if !modifiers.shift => Some(BrowserAction::ToggleFavorite),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(key: &str, control: bool, shift: bool) -> Keystroke {
        Keystroke {
            key: key.to_owned(),
            modifiers: Modifiers {
                control,
                shift,
                ..Modifiers::default()
            },
            ..Keystroke::default()
        }
    }

    #[test]
    fn menu_state_sans_page_ne_grise_que_les_actions_page() {
        let states = menu_state(false, ZOOM_DEFAULT, false);
        assert!(states.new_tab);
        assert!(states.device_emulation);
        assert!(states.history);
        assert!(states.clear_storage);
        assert!(states.browser_settings);
        assert!(!states.zoom_in);
        assert!(!states.zoom_out);
        assert!(!states.reset_zoom);
        assert!(!states.find_in_page);
        assert!(!states.open_external);
        assert!(!states.toggle_favorite);
    }

    #[test]
    fn menu_state_page_charge_active_tout_sauf_zoom_bords() {
        let states = menu_state(true, ZOOM_DEFAULT, false);
        assert!(states.zoom_in);
        assert!(states.zoom_out);
        // À 100 % le reset n'a rien à faire.
        assert!(!states.reset_zoom);
        assert!(states.find_in_page);
        assert!(states.open_external);
        assert!(states.toggle_favorite);
    }

    #[test]
    fn menu_state_borne_les_niveaux_de_zoom() {
        let max = menu_state(true, ZOOM_MAX, false);
        assert!(!max.zoom_in);
        assert!(max.zoom_out);
        assert!(max.reset_zoom);

        let min = menu_state(true, ZOOM_MIN, false);
        assert!(min.zoom_in);
        assert!(!min.zoom_out);
        assert!(min.reset_zoom);

        let zoomed = menu_state(true, 1.5, false);
        assert!(zoomed.zoom_in);
        assert!(zoomed.zoom_out);
        assert!(zoomed.reset_zoom);
    }

    #[test]
    fn menu_items_couvrent_les_six_entrees_dans_l_ordre() {
        let states = menu_state(true, ZOOM_DEFAULT, false);
        let items = all_menu_items(&states, false);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_ref()).collect();
        assert_eq!(
            labels,
            [
                "Zoom in",
                "Zoom out",
                "Reset zoom",
                "Copy link",
                "Open in browser",
                "Clear history",
            ]
        );
        // Un séparateur ouvre chaque groupe à partir du second.
        let separated: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.separated)
            .map(|(ix, _)| ix)
            .collect();
        assert_eq!(separated, [3, 5]);
    }

    #[test]
    fn shortcut_nouv_tab_et_devtools() {
        assert_eq!(
            browser_shortcut(&key("t", true, false)),
            Some(BrowserAction::NewTab)
        );
        // Cmd (platform) marche aussi.
        let cmd_t = Keystroke {
            modifiers: Modifiers {
                platform: true,
                ..Modifiers::default()
            },
            ..key("t", false, false)
        };
        assert_eq!(browser_shortcut(&cmd_t), Some(BrowserAction::NewTab));
        assert_eq!(
            browser_shortcut(&Keystroke {
                key: "f12".into(),
                ..Keystroke::default()
            }),
            Some(BrowserAction::ToggleConsole)
        );
        assert_eq!(
            browser_shortcut(&key("i", true, true)),
            Some(BrowserAction::ToggleConsole)
        );
    }

    #[test]
    fn shortcut_zoom_find_historique_favoris() {
        assert_eq!(
            browser_shortcut(&key("=", true, false)),
            Some(BrowserAction::ZoomIn)
        );
        assert_eq!(
            browser_shortcut(&key("-", true, false)),
            Some(BrowserAction::ZoomOut)
        );
        assert_eq!(
            browser_shortcut(&key("0", true, false)),
            Some(BrowserAction::ResetZoom)
        );
        assert_eq!(
            browser_shortcut(&key("f", true, false)),
            Some(BrowserAction::FindInPage)
        );
        assert_eq!(
            browser_shortcut(&key("h", true, false)),
            Some(BrowserAction::History)
        );
        assert_eq!(
            browser_shortcut(&key("d", true, false)),
            Some(BrowserAction::ToggleFavorite)
        );
    }

    #[test]
    fn shortcut_ignore_le_reste() {
        // Touche seule, mauvais modif, ou combinaison non navigateur.
        assert_eq!(browser_shortcut(&key("t", false, false)), None);
        assert_eq!(browser_shortcut(&key("t", true, true)), None);
        assert_eq!(browser_shortcut(&key("x", true, false)), None);
        let alt_t = Keystroke {
            modifiers: Modifiers {
                control: true,
                alt: true,
                ..Modifiers::default()
            },
            ..key("t", false, false)
        };
        assert_eq!(browser_shortcut(&alt_t), None);
        // Ctrl+Shift+I avec Alt en plus ne déclenche rien.
        let alt_i = Keystroke {
            modifiers: Modifiers {
                control: true,
                shift: true,
                alt: true,
                ..Modifiers::default()
            },
            ..key("i", false, false)
        };
        assert_eq!(browser_shortcut(&alt_i), None);
    }
}
