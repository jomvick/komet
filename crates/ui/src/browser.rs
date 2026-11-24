mod cdp;
mod clear;
mod downloads;
mod element;
mod history;
mod menu;
mod network;
mod process;
mod session;

pub use clear::ClearSelection;
pub use element::{ElementContext, ElementHover};
pub use history::{BrowserHistory, clear_history_file, clear_storage_files};
pub use menu::{BrowserAction, MenuItem, MenuStates, action_shipped};

// Couche session (contrôleur CDP) extraite dans `browser/session.rs` —
// l'API vue par la surface et les tests reste inchangée. `BrowserCommand`/
// `BrowserEvent` restent exportés publiquement comme avant l'extraction.
pub use session::{BrowserCommand, BrowserEvent};
use session::{CommandPipe, WakeRx, WakeTx, run_client};

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Element, Empty, Entity, FocusHandle,
    Focusable, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    MouseButton, ObjectFit, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, Window, div, img, prelude::*, px,
};

pub use cdp::{ConsoleLog, FindPageResult, OverlayElement, cdp_modifiers};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::motion;
use crate::popover;
use crate::settings::BrowserPrefs;

const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);

/// Paliers de zoom (fraction de la taille CSS) : Ctrl+= / Ctrl+- / Ctrl+0.
/// `effective_viewport` (PR zoom) divise la zone dispo par le niveau.
pub const ZOOM_MIN: f32 = 0.25;
pub const ZOOM_MAX: f32 = 5.0;
pub const ZOOM_DEFAULT: f32 = 1.0;
/// Paliers de zoom (fraction) : Ctrl+= / Ctrl+- / Ctrl+0.
pub const ZOOM_LEVELS: [f32; 17] = [
    0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0,
];

/// Presets d'émulation (slug persisté dans `BrowserPrefs.emulationPreset`,
/// libellé affiché dans Settings → Browser et la barre d'émulation PR 7).
/// `"off"` = pas d'émulation ; les dimensions/user-agent arrivent avec la PR
/// device emulation — le slug fait foi.
pub const EMULATION_PRESETS: &[(&str, &str)] = &[
    ("off", "Off"),
    ("responsive", "Responsive"),
    ("iphone-se", "iPhone SE · 375×667"),
    ("iphone-14-pro", "iPhone 14 Pro · 393×852"),
    ("pixel-7", "Pixel 7 · 412×915"),
    ("ipad-mini", "iPad Mini · 768×1024"),
    ("laptop", "Laptop · 1280×800"),
];

/// Device émulé : viewport CSS fixe, rendu dans un cadre centré mis à
/// l'échelle (`fit = min(1, zone/device)`). `mobile`/`touch`/`user_agent`
/// partent vers CDP (`Emulation.*`) avec le changement de preset.
#[derive(Debug, Clone)]
pub struct EmulatedDevice {
    /// Slug du preset d'origine (`"responsive"` pour une taille libre).
    pub slug: &'static str,
    pub label: String,
    /// Viewport CSS nominal (avant zoom).
    pub width: u32,
    pub height: u32,
    pub mobile: bool,
    pub touch: bool,
    pub user_agent: Option<&'static str>,
}

const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const IPAD_UA: &str = "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const PIXEL_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36";

/// Dimensions/User-Agent des presets de `EMULATION_PRESETS` (hors `"off"`).
/// `"responsive"` n'a pas de dimensions propres : elles viennent des champs
/// L × H de la barre d'émulation.
pub fn emulation_device(slug: &str, custom: (u32, u32)) -> Option<EmulatedDevice> {
    let (label, width, height, mobile, touch, user_agent) = match slug {
        "responsive" => (
            "Responsive".to_string(),
            custom.0,
            custom.1,
            false,
            false,
            None,
        ),
        "iphone-se" => (
            "iPhone SE · 375×667".into(),
            375,
            667,
            true,
            true,
            Some(IPHONE_UA),
        ),
        "iphone-14-pro" => (
            "iPhone 14 Pro · 393×852".into(),
            393,
            852,
            true,
            true,
            Some(IPHONE_UA),
        ),
        "pixel-7" => (
            "Pixel 7 · 412×915".into(),
            412,
            915,
            true,
            true,
            Some(PIXEL_UA),
        ),
        "ipad-mini" => (
            "iPad Mini · 768×1024".into(),
            768,
            1024,
            true,
            true,
            Some(IPAD_UA),
        ),
        "laptop" => ("Laptop · 1280×800".into(), 1280, 800, false, false, None),
        _ => return None,
    };
    Some(EmulatedDevice {
        slug: match slug {
            "responsive" => "responsive",
            "iphone-se" => "iphone-se",
            "iphone-14-pro" => "iphone-14-pro",
            "pixel-7" => "pixel-7",
            "ipad-mini" => "ipad-mini",
            _ => "laptop",
        },
        label,
        width,
        height,
        mobile,
        touch,
        user_agent,
    })
}

/// Transform écran ↔ page : `ox`/`oy` = offset du cadre émulé dans la zone,
/// `fw`/`fh` = taille écran du cadre, `div` = pixels écran par pixel CSS
/// (`zoom × fit`). Sans émulation : identité × zoom.
#[derive(Debug, Clone, Copy)]
struct ViewTransform {
    ox: f32,
    oy: f32,
    fw: f32,
    fh: f32,
    div: f32,
}

/// Viewport effectif envoyé à Chrome : dimensions CSS = zone ÷ zoom,
/// `deviceScaleFactor` = échelle fenêtre × zoom — l'image reçue garde la
/// taille de la zone, donc le contenu paraît agrandi (vrai zoom navigateur).
pub fn effective_viewport(avail: (f32, f32), zoom: f32, device_scale: f32) -> (u32, u32, f32) {
    let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
    let css_w = (avail.0 / zoom).round().max(1.0) as u32;
    let css_h = (avail.1 / zoom).round().max(1.0) as u32;
    (css_w, css_h, device_scale * zoom)
}

/// Palier de zoom suivant `dir` (>0 monte, <0 descend) dans `ZOOM_LEVELS`,
/// borné aux extrêmes.
fn zoom_step_level(zoom: f32, dir: i32) -> f32 {
    if dir >= 0 {
        ZOOM_LEVELS
            .iter()
            .copied()
            .find(|level| *level > zoom + 0.0001)
            .unwrap_or(ZOOM_MAX)
    } else {
        ZOOM_LEVELS
            .iter()
            .copied()
            .rev()
            .find(|level| *level < zoom - 0.0001)
            .unwrap_or(ZOOM_MIN)
    }
}

/// Bout de pipe en écriture : chaque commande envoyée au thread CDP écrit un
/// octet pour réveiller son poll(2) immédiatement (zéro latence, zéro timer).
#[cfg(target_os = "linux")]
struct FdGuard(i32);

#[cfg(target_os = "linux")]
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Hors Linux il n'y a pas de self-pipe : le type existe pour que le champ
/// de la surface reste le même partout, sans jamais être construit.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
struct FdGuard;

/// Événements surface → shell : le panneau demande au shell d'ouvrir un
/// nouvel onglet, la page settings, d'indexer un élément dans le composer ou
/// le panneau d'historique.
pub enum BrowserSurfaceEvent {
    NewTab,
    OpenSettings,
    AddElement(Box<ElementContext>),
    OpenHistoryPanel,
    /// Le preset d'émulation a changé dans la barre device — le shell le
    /// persiste dans `BrowserPrefs.emulation_preset` (`None` = off).
    EmulationChanged(Option<String>),
    /// « Clear Storage » confirmé avec « History » coché : le journal est
    /// partagé entre surfaces (même data_dir), le shell purge la mémoire
    /// de TOUTES les surfaces — les favoris survivent.
    HistoryCleared,
}

impl gpui::EventEmitter<BrowserSurfaceEvent> for BrowserSurface {}

/// Filtre du tiroir DevTools (Console).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ConsoleFilter {
    #[default]
    All,
    Errors,
    Warnings,
}

/// `true` si une ligne de console passe le filtre.
fn console_log_visible(level: &str, filter: ConsoleFilter) -> bool {
    match filter {
        ConsoleFilter::All => true,
        ConsoleFilter::Errors => level == "error",
        ConsoleFilter::Warnings => matches!(level, "warning" | "warn"),
    }
}

/// Marqueur du drag de la poignée de redimensionnement du tiroir DevTools
/// (pattern `on_drag`/`on_drag_move` de gpui, comme le panneau terminal).
struct ConsoleDrawerResize;

/// Payload invisible du drag de la poignée (miroir du `DragGhost` du shell).
struct ConsoleDragGhost;

impl Render for ConsoleDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Onglet du panneau History/Favorites/Downloads/Network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    History,
    Favorites,
    Downloads,
    Network,
}

/// Case à cocher du dialogue « Clear Storage ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClearFlag {
    Cookies,
    Cache,
    SiteData,
    History,
}

/// Panneau « History | Favorites » : vue native à la place de la page,
/// avec champ de recherche et regroupement par jour.
struct HistoryPanel {
    tab: PanelTab,
    search: Entity<ComposerInput>,
    /// Miroir du texte du champ (filtre titre/URL/hôte).
    query: String,
    /// Menu « Clear… » ouvert (dernière heure / aujourd'hui / tout).
    clear_open: bool,
    _search_subscription: gpui::Subscription,
}

pub struct BrowserSurface {
    commands: CommandPipe,
    events: Receiver<BrowserEvent>,
    address: Entity<ComposerInput>,
    _address_subscription: gpui::Subscription,
    console_input: Entity<ComposerInput>,
    _console_subscription: gpui::Subscription,
    page_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    page_focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    retired_images: Vec<Arc<RenderImage>>,
    title: String,
    url: String,
    is_loading: bool,
    can_back: bool,
    can_forward: bool,
    error: Option<String>,
    elements: Vec<OverlayElement>,
    show_overlay: bool,
    /// Mode « Add element » : le clic ne part plus dans la page, le survol
    /// affiche contour + pastille, Échap quitte (touche N = badges numérotés).
    select_mode: bool,
    select_hover: Option<ElementHover>,
    /// Throttle des `ProbeAt` (~40 ms → 25 sondes/seconde max).
    last_probe: Cell<Instant>,
    show_console: bool,
    console_logs: VecDeque<ConsoleLog>,
    /// Filtre du tiroir DevTools : Tout / Erreurs / Avertissements.
    console_filter: ConsoleFilter,
    /// Hauteur du tiroir DevTools (défaut 220, poignée de redimensionnement,
    /// min 120, max 60 % de la zone).
    console_height: f32,
    /// (y de départ, hauteur de départ) pendant le drag de la poignée.
    console_drag_anchor: Option<(f32, f32)>,
    status_toast: Option<String>,
    /// The "…" overflow menu's open flag.
    dots_open: bool,
    last_viewport: Cell<(u32, u32)>,
    visible: bool,
    /// resize débouncé : la plus récente taille + scale à envoyer
    pending_resize: Rc<Cell<Option<(u32, u32, f32)>>>,
    resize_gen: Rc<Cell<u64>>,
    resize_fired: Rc<Cell<u64>>,
    resize_scheduled: Rc<Cell<bool>>,
    /// Historique + favoris (JSONL sous `{data_dir}/browser/`) — `None`
    /// quand aucun data_dir n'est fourni (tests, environnements nus).
    history: Option<BrowserHistory>,
    /// Panneau History/Favorites affiché à la place de la page.
    panel: Option<HistoryPanel>,
    /// Téléchargements suivis (GUID CDP → item), du début à l'ouverture.
    downloads: Vec<downloads::DownloadItem>,
    /// Journal réseau (requêtes/réponses/erreurs, plafonné).
    network: network::NetworkLog,
    /// Suggestions de la barre d'adresse (lignes historique + « Search … »).
    suggestions: Vec<history::Suggestion>,
    /// Ligne surlignée (0 = première) pour ↑/↓ + Entrée.
    suggestion_sel: usize,
    /// Réglages Settings → Browser (recherche, historique, zoom, captures,
    /// émulation) — `update_prefs` les répercute sur les surfaces ouvertes.
    prefs: BrowserPrefs,
    /// Zoom courant (fraction CSS, 1.0 = 100 %) — initialisé depuis
    /// `prefs.zoom_default`, ajusté par Ctrl+=/-/0 et le menu.
    zoom: f32,
    /// Mémoire du zoom par hôte (réglage « Remember zoom per site ») :
    /// hôte → dernier niveau choisi sur cette surface.
    zoom_by_host: HashMap<String, f32>,
    /// Hôte auquel `zoom` se rapporte actuellement (`None` = pas de page).
    zoom_host: Option<String>,
    /// Barre « Find in Page » flottante au-dessus de la page.
    find_open: bool,
    find_input: Entity<ComposerInput>,
    /// Miroir du texte du champ de recherche.
    find_query: String,
    /// Occurrences et index courant renvoyés par le script de surlignage.
    find_count: usize,
    find_index: i64,
    _find_subscription: gpui::Subscription,
    /// Device emulation active (`None` = viewport fluide).
    emulation: Option<EmulatedDevice>,
    /// Barre device visible (menu « Device Emulation » ou preset choisi).
    emu_open: bool,
    /// Popover des presets ouvert.
    emu_preset_open: bool,
    /// Champs L × H de la barre (taille libre → preset « responsive »).
    emu_w: Entity<ComposerInput>,
    emu_h: Entity<ComposerInput>,
    /// `true` pendant la synchro programmatique des champs — `set_text`
    /// émet `Edited`, sans ce garde-fou chaque preset retomberait sur
    /// « responsive ».
    emu_sync: bool,
    _emu_subscriptions: Vec<gpui::Subscription>,
    /// Dialogue « Clear Storage » : visible + cases cochées (défaut =
    /// cookies + cache, voir [`ClearSelection::DEFAULT`]).
    clear_open: bool,
    clear_sel: ClearSelection,
    /// Dernière `window.scale_factor()` vue au render — `push_resize`
    /// (changement de page/zoom) en a besoin hors contexte window.
    last_scale: Cell<f32>,
}

impl BrowserSurface {
    pub fn new(
        url: String,
        profile_key: String,
        data_dir: Option<PathBuf>,
        prefs: BrowserPrefs,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = cx.new(|cx| ComposerInput::new("Search or enter URL", cx));
        address.update(cx, |input, cx| {
            input.set_text(url.clone(), cx);
            // Échap ferme les suggestions ; ↑/↓ les naviguent.
            input.enable_escape_cancel();
            input.set_popup_nav(true);
        });

        let console_input = cx.new(|cx| ComposerInput::new("Evaluate JavaScript...", cx));

        // « Find in Page » : contexte PaletteSearch → ↑↓/⏎/Échap non liés
        // dans l'input, ils remontent à la barre flottante (comme les
        // palettes). `Edited` relance la recherche à chaque frappe.
        let find_input =
            cx.new(|cx| ComposerInput::with_context("Find in page…", "PaletteSearch", cx));
        let find_subscription = cx.subscribe(
            &find_input,
            |this, input, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    this.find_query = input.read(cx).text().trim().to_owned();
                    this.run_find(0, cx);
                    cx.notify();
                }
            },
        );

        // Barre d'émulation : champs L × H en contexte Composer (⏎ =
        // `Submitted`). `Edited` applique la taille libre en direct — comme
        // les champs d'un device toolbar, toute saisie bascule sur
        // « responsive ».
        let emu_w = cx.new(|cx| ComposerInput::new("393", cx));
        let emu_h = cx.new(|cx| ComposerInput::new("852", cx));
        let emu_w_sub = cx.subscribe(&emu_w, |this, _, event: &ComposerInputEvent, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.emu_apply_size_fields(cx);
            }
        });
        let emu_h_sub = cx.subscribe(&emu_h, |this, _, event: &ComposerInputEvent, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.emu_apply_size_fields(cx);
            }
        });

        // self-pipe : réveil instantané du poll(2) du thread CDP à chaque
        // commande ; les deux bouts sont non bloquants (pipe plein = inutile
        // d'écrire, le CDP est déjà réveillé par les octets en attente).
        #[cfg(target_os = "linux")]
        let (wake_read, wake_write) = {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) } == 0 {
                (Some(fds[0]), Some(fds[1]))
            } else {
                (None, None)
            }
        };
        #[cfg(not(target_os = "linux"))]
        let (wake_read, wake_write): (Option<i32>, Option<i32>) = (None, None);
        #[cfg(target_os = "linux")]
        let _wake_write = wake_write.map(FdGuard);
        #[cfg(not(target_os = "linux"))]
        let _wake_write: Option<FdGuard> = None;

        let (command_tx, command_rx) = mpsc::channel();
        let commands = CommandPipe {
            tx: command_tx,
            wake_fd: wake_write,
        };
        let (event_tx, event_rx) = mpsc::sync_channel(8);
        let (wake_tx, wake_rx): (WakeTx, WakeRx) = futures::channel::mpsc::unbounded();
        let initial_url = url.clone();
        let initial_download_dir = downloads::resolve_download_dir(prefs.download_dir.as_deref());
        {
            let startup_events = event_tx.clone();
            let spawn_wake = wake_tx.clone();
            if let Err(error) = thread::Builder::new()
                .name("komet-cdp-browser".into())
                .spawn(move || {
                    run_client(
                        command_rx,
                        event_tx,
                        &spawn_wake,
                        wake_read,
                        initial_url,
                        profile_key,
                        initial_download_dir,
                    )
                })
            {
                let _ = startup_events.send(BrowserEvent::Failed(format!(
                    "Could not start browser CDP client: {error}"
                )));
                let _ = wake_tx.unbounded_send(());
            }
        }

        let address_subscription = cx.subscribe(
            &address,
            |this, _input, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Submitted => {
                    // Entrée avec une suggestion surlignée → on y va ; sinon
                    // le texte tapé est soumis (URL ou recherche).
                    if this.suggestion_sel < this.suggestions.len() {
                        let target = this.suggestions[this.suggestion_sel].target.clone();
                        this.navigate_to(target, cx);
                    } else {
                        this.submit_address(cx);
                    }
                }
                ComposerInputEvent::Edited => {
                    this.refresh_suggestions(cx);
                    cx.notify();
                }
                ComposerInputEvent::PopupNavigate(delta) => {
                    if !this.suggestions.is_empty() {
                        let len = this.suggestions.len() as isize;
                        let next = (this.suggestion_sel as isize + delta).rem_euclid(len);
                        this.suggestion_sel = next as usize;
                        cx.notify();
                    }
                }
                ComposerInputEvent::Cancelled => {
                    this.suggestions.clear();
                    cx.notify();
                }
                _ => {}
            },
        );

        let console_subscription = cx.subscribe(
            &console_input,
            |this, input, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    let expr = input.read(cx).text().trim().to_owned();
                    if !expr.is_empty() {
                        this.commands.send(BrowserCommand::Evaluate(expr.clone()));
                        push_console(
                            &mut this.console_logs,
                            ConsoleLog {
                                level: "input".into(),
                                text: format!("> {expr}"),
                            },
                        );
                        this.console_input
                            .update(cx, |input, cx| input.set_text("", cx));
                    }
                    cx.notify();
                }
            },
        );

        let initial_zoom = prefs.zoom_default.clamp(ZOOM_MIN, ZOOM_MAX);
        // Preset d'émulation par défaut (Settings → Browser), résolu avant
        // le déplacement de `prefs` dans la struct.
        let initial_emulation = prefs
            .emulation_preset
            .as_deref()
            .and_then(|slug| emulation_device(slug, (800, 600)));
        let emu_open_initial =
            prefs.emulation_preset.is_some() && prefs.emulation_preset.as_deref() != Some("off");
        let mut browser = Self {
            commands,
            events: event_rx,
            address,
            _address_subscription: address_subscription,
            console_input,
            _console_subscription: console_subscription,
            page_bounds: Rc::new(Cell::new(None)),
            page_focus: cx.focus_handle(),
            image: None,
            retired_images: Vec::new(),
            // Empty until the page reports a title — `tab_title` falls back
            // to the URL's host, then "Browser".
            title: String::new(),
            url,
            is_loading: false,
            can_back: false,
            can_forward: false,
            error: None,
            elements: Vec::new(),
            show_overlay: false,
            select_mode: false,
            select_hover: None,
            last_probe: Cell::new(Instant::now() - Duration::from_secs(1)),
            show_console: false,
            console_logs: VecDeque::new(),
            console_filter: ConsoleFilter::All,
            console_height: 220.0,
            console_drag_anchor: None,
            status_toast: None,
            dots_open: false,
            last_viewport: Cell::new((0, 0)),
            visible: true,
            pending_resize: Rc::new(Cell::new(None)),
            resize_gen: Rc::new(Cell::new(0)),
            resize_fired: Rc::new(Cell::new(0)),
            resize_scheduled: Rc::new(Cell::new(false)),
            history: data_dir.map(|dir| {
                let mut store = BrowserHistory::load(&dir);
                store.set_retention_days(prefs.history_retention_days);
                store
            }),
            panel: None,
            downloads: Vec::new(),
            network: network::NetworkLog::default(),
            suggestions: Vec::new(),
            suggestion_sel: 0,
            prefs,
            zoom: initial_zoom,
            zoom_by_host: HashMap::new(),
            zoom_host: None,
            find_open: false,
            find_input,
            find_query: String::new(),
            find_count: 0,
            find_index: -1,
            _find_subscription: find_subscription,
            // Preset d'émulation par défaut (Settings → Browser) : appliqué
            // à la surface dès sa création.
            emulation: initial_emulation.clone(),
            emu_open: emu_open_initial,
            emu_preset_open: false,
            emu_w,
            emu_h,
            emu_sync: false,
            _emu_subscriptions: vec![emu_w_sub, emu_h_sub],
            clear_open: false,
            clear_sel: ClearSelection::DEFAULT,
            last_scale: Cell::new(1.0),
        };

        // Preset par défaut : UA/touch/mobile partent au thread CDP dès la
        // file de commandes (ordonné avant le premier Resize), et les champs
        // L × H reflètent l'appareil.
        if let Some(device) = initial_emulation.clone() {
            browser.emu_sync = true;
            browser.emu_w.update(cx, |input, cx| {
                input.set_text(device.width.to_string(), cx);
            });
            browser.emu_h.update(cx, |input, cx| {
                input.set_text(device.height.to_string(), cx);
            });
            browser.emu_sync = false;
            browser.commands.send(BrowserCommand::SetEmulation {
                device: Some(device),
            });
        }

        // Réveil piloté par le thread CDP (canal async) : plus de timer.
        cx.spawn(async move |this, cx| {
            let mut wake_rx = wake_rx;
            while wake_rx.next().await.is_some()
                && this.update(cx, |view, cx| view.receive_events(cx)).is_ok()
            {}
        })
        .detach();
        browser
    }

    /// Répercute les réglages Settings → Browser sur la surface ouverte.
    /// Recherche/captures/page d'accueil sont lus à l'usage via `prefs` ;
    /// la rétention et le zoom par défaut sont reappliqués tout de suite.
    pub fn update_prefs(&mut self, prefs: BrowserPrefs, cx: &mut Context<Self>) {
        // Dossier de téléchargements modifié : ré-appliqué sur la session
        // ouverte (`Browser.setDownloadBehavior`), pas seulement au prochain
        // démarrage de session.
        let dir = downloads::resolve_download_dir(prefs.download_dir.as_deref());
        self.commands.send(BrowserCommand::SetDownloadDir(
            dir.to_string_lossy().into_owned(),
        ));
        if let Some(history) = self.history.as_mut() {
            history.set_retention_days(prefs.history_retention_days);
        }
        // Sans page chargée, le zoom par défaut s'applique tout de suite ;
        // sur une page, le niveau courant (ou celui de l'hôte) est conservé.
        if self.zoom_host.is_none() {
            let default_zoom = prefs.zoom_default.clamp(ZOOM_MIN, ZOOM_MAX);
            if (self.zoom - default_zoom).abs() > 0.0001 {
                self.zoom = default_zoom;
                self.push_resize(cx);
            }
        }
        self.prefs = prefs;
        cx.notify();
    }

    // ---- zoom ----

    /// Positionne le zoom courant (borné), le mémorise pour l'hôte si
    /// « Remember zoom per site » est actif, puis renvoie le viewport.
    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if (self.zoom - zoom).abs() < 0.0001 {
            return;
        }
        self.zoom = zoom;
        if self.prefs.remember_zoom
            && let Some(host) = self.zoom_host.clone()
        {
            self.zoom_by_host.insert(host, zoom);
        }
        self.push_resize(cx);
        cx.notify();
    }

    /// Ctrl+= / Ctrl+- : monte/descend d'un palier dans `ZOOM_LEVELS`.
    fn zoom_by(&mut self, dir: i32, cx: &mut Context<Self>) {
        self.set_zoom(zoom_step_level(self.zoom, dir), cx);
    }

    /// Changement de page : si « Remember zoom per site » est actif, le
    /// niveau de l'ancien hôte est archivé et celui du nouvel hôte restauré
    /// (ou le zoom par défaut). Sinon le zoom courant suit la surface.
    fn apply_host_zoom(&mut self, cx: &mut Context<Self>) {
        let host = host_of(&self.url);
        if host == self.zoom_host {
            return;
        }
        let old = self.zoom_host.take();
        self.zoom_host = host.clone();
        if !self.prefs.remember_zoom {
            return;
        }
        if let Some(old) = old {
            self.zoom_by_host.insert(old, self.zoom);
        }
        let restored = host
            .as_ref()
            .and_then(|h| self.zoom_by_host.get(h).copied())
            .unwrap_or(self.prefs.zoom_default)
            .clamp(ZOOM_MIN, ZOOM_MAX);
        if (restored - self.zoom).abs() > 0.0001 {
            self.zoom = restored;
            self.push_resize(cx);
        }
    }

    /// Force un envoi de `Resize` avec le viewport effectif courant —
    /// utilisé quand le zoom change sans que la zone ait bougé.
    fn push_resize(&mut self, cx: &mut Context<Self>) {
        if let Some(bounds) = self.page_bounds.get() {
            let w = f32::from(bounds.size.width) as u32;
            let h = f32::from(bounds.size.height) as u32;
            if w > 100 && h > 100 {
                self.pending_resize.set(Some((w, h, self.last_scale.get())));
                let generation = self.resize_gen.get() + 1;
                self.resize_gen.set(generation);
                if !self.resize_scheduled.replace(true) {
                    self.schedule_resize_flush(cx);
                }
            }
        }
    }

    // ---- Find in Page ----

    /// Ouvre la barre de recherche et y déplace le focus.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_page() {
            return;
        }
        self.find_open = true;
        let handle = self.find_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.run_find(0, cx);
        cx.notify();
    }

    /// Ferme la barre : retire surlignages/style côté page, rend le focus
    /// à la page pour que les raccourcis repartent dans Chrome.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_open {
            return;
        }
        self.find_open = false;
        self.find_count = 0;
        self.find_index = -1;
        self.commands.send(BrowserCommand::ClearFind);
        window.focus(&self.page_focus, cx);
        cx.notify();
    }

    /// Envoie la recherche courante au thread CDP (`step` 0 = rescan,
    /// ±1 = occurrence suivante/précédente sur la même requête).
    fn run_find(&mut self, step: i32, cx: &mut Context<Self>) {
        if !self.find_open {
            return;
        }
        self.commands.send(BrowserCommand::FindInPage {
            query: self.find_query.clone(),
            step,
        });
        cx.notify();
    }

    /// ⏎ / ⇧⏎ / Échap dans la barre : la recherche est en contexte
    /// `PaletteSearch`, ces touches ne sont pas liées dans l'input et
    /// remontent ici par `on_key_down`.
    fn on_find_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "enter" => {
                let step = if event.keystroke.modifiers.shift {
                    -1
                } else {
                    1
                };
                if !self.find_query.is_empty() {
                    self.run_find(step, cx);
                }
                cx.stop_propagation();
            }
            "escape" => {
                self.close_find(window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // ---- Device emulation ----

    /// Applique (ou retire, `None`) l'émulation : commande CDP + viewport
    /// + persistance du slug via `BrowserSurfaceEvent::EmulationChanged`.
    fn set_emulation(&mut self, device: Option<EmulatedDevice>, cx: &mut Context<Self>) {
        if let Some(dev) = &device {
            // Les champs L × H reflètent toujours l'appareil courant.
            self.emu_sync = true;
            self.emu_w.update(cx, |input, cx| {
                input.set_text(dev.width.to_string(), cx);
            });
            self.emu_h.update(cx, |input, cx| {
                input.set_text(dev.height.to_string(), cx);
            });
            self.emu_sync = false;
        }
        let slug = device.as_ref().map(|d| d.slug.to_string());
        self.emulation = device.clone();
        self.commands.send(BrowserCommand::SetEmulation { device });
        self.push_resize(cx);
        cx.emit(BrowserSurfaceEvent::EmulationChanged(slug));
        cx.notify();
    }

    /// Choix d'un preset dans le popover (`"off"` = fin d'émulation).
    fn emu_pick_preset(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.emu_preset_open = false;
        if slug == "off" {
            self.set_emulation(None, cx);
            return;
        }
        let custom = self.emu_custom_size(cx);
        if let Some(device) = emulation_device(slug, custom) {
            self.set_emulation(Some(device), cx);
        }
    }

    /// Dimensions saisies dans les champs L × H ; repli sur l'appareil
    /// courant puis 800×600.
    fn emu_custom_size(&self, cx: &App) -> (u32, u32) {
        let read = |input: &Entity<ComposerInput>| input.read(cx).text().trim().parse::<u32>().ok();
        match (read(&self.emu_w), read(&self.emu_h)) {
            (Some(w), Some(h)) => (w.clamp(100, 4096), h.clamp(100, 4096)),
            _ => self
                .emulation
                .as_ref()
                .map(|d| (d.width, d.height))
                .unwrap_or((800, 600)),
        }
    }

    /// Champs L × H édités → preset « responsive » à la taille saisie.
    /// Ignoré pendant la synchro programmatique (`emu_sync`) et sur les
    /// devices mobiles à dimensions fixes.
    fn emu_apply_size_fields(&mut self, cx: &mut Context<Self>) {
        if self.emu_sync {
            return;
        }
        let editable = self
            .emulation
            .as_ref()
            .is_some_and(|d| !d.mobile || d.slug == "responsive" || d.slug == "laptop");
        if !editable {
            return;
        }
        let w = self.emu_w.read(cx).text().trim().parse::<u32>().ok();
        let h = self.emu_h.read(cx).text().trim().parse::<u32>().ok();
        let (Some(w), Some(h)) = (w, h) else { return };
        let (w, h) = (w.clamp(100, 4096), h.clamp(100, 4096));
        // Dimensions inchangées → pas de re-émission (set_text émet Edited).
        if let Some(d) = &self.emulation
            && d.width == w
            && d.height == h
        {
            return;
        }
        if let Some(device) = emulation_device("responsive", (w, h)) {
            self.set_emulation(Some(device), cx);
        }
    }

    /// Rotation : échange L et H de l'appareil courant.
    fn emu_rotate(&mut self, cx: &mut Context<Self>) {
        let Some(mut dev) = self.emulation.clone() else {
            return;
        };
        std::mem::swap(&mut dev.width, &mut dev.height);
        dev.label = format!("{}×{}", dev.width, dev.height);
        self.set_emulation(Some(dev), cx);
    }

    /// Transform écran ↔ page pour les événements souris : sans émulation,
    /// `div = zoom` ; avec, le cadre device est centré et mis à l'échelle.
    fn view_transform(&self) -> ViewTransform {
        let zone = self
            .page_bounds
            .get()
            .map(|b| (f32::from(b.size.width), f32::from(b.size.height)))
            .unwrap_or((0.0, 0.0));
        let zoom = self.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        match &self.emulation {
            Some(dev) if dev.width > 0 && dev.height > 0 && zone.0 > 1.0 && zone.1 > 1.0 => {
                let fit = (zone.0 / dev.width as f32)
                    .min(zone.1 / dev.height as f32)
                    .clamp(0.05, 1.0);
                let fw = dev.width as f32 * fit;
                let fh = dev.height as f32 * fit;
                ViewTransform {
                    ox: ((zone.0 - fw) / 2.0).max(0.0),
                    oy: ((zone.1 - fh) / 2.0).max(0.0),
                    fw,
                    fh,
                    div: zoom * fit,
                }
            }
            _ => ViewTransform {
                ox: 0.0,
                oy: 0.0,
                fw: zone.0,
                fh: zone.1,
                div: zoom,
            },
        }
    }

    /// Viewport CSS + deviceScaleFactor à envoyer à Chrome : sans émulation
    /// c'est `effective_viewport(zone)` ; avec, le viewport CSS est la taille
    /// du device ÷ zoom et le dsf compense (l'image reste à la taille du cadre).
    fn cdp_metrics(&self, zone: (u32, u32), scale: f32) -> (u32, u32, f32) {
        let zoom = self.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if let Some(dev) = &self.emulation {
            let css_w = (dev.width as f32 / zoom).round().max(1.0) as u32;
            let css_h = (dev.height as f32 / zoom).round().max(1.0) as u32;
            (css_w, css_h, scale * zoom)
        } else {
            effective_viewport((zone.0 as f32, zone.1 as f32), zoom, scale)
        }
    }

    /// Le panneau s'affiche ou se cache. Caché, la session CDP se gare :
    /// stopScreencast + attente bloquante (zéro réveil tant que caché).
    pub fn set_visible(&mut self, visible: bool) {
        if self.visible != visible {
            self.visible = visible;
            self.commands.send(BrowserCommand::SetVisible(visible));
        }
    }

    /// Débounce 150 ms : une seule commande Resize après la fin du drag ;
    /// re-planifié tant que la taille bouge pendant l'attente.
    fn schedule_resize_flush(&self, cx: &mut Context<Self>) {
        let pending = self.pending_resize.clone();
        let fired = self.resize_fired.clone();
        let scheduled = self.resize_scheduled.clone();
        let commands = self.commands.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        if view.resize_gen.get() != fired.get() {
                            fired.set(view.resize_gen.get());
                            false
                        } else {
                            scheduled.set(false);
                            if let Some((w, h, scale)) = pending.take() {
                                // Zone ÷ zoom = viewport CSS (ou taille du
                                // device émulé) ; le deviceScaleFactor
                                // compense pour que l'image garde sa taille.
                                let (css_w, css_h, dsf) = view.cdp_metrics((w, h), scale);
                                commands.send(BrowserCommand::Resize {
                                    width: css_w,
                                    height: css_h,
                                    scale: dsf,
                                });
                                cx.notify();
                            }
                            true
                        }
                    })
                    .unwrap_or(true)
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn submit_address(&mut self, cx: &mut Context<Self>) {
        let value = self.address.read(cx).text().to_owned();
        self.navigate_to(value, cx);
    }

    /// Navigue vers `value` (URL ou recherche) — utilisé par la barre
    /// d'adresse, les suggestions, le panneau History et l'état vide.
    pub fn navigate_to(&mut self, value: String, cx: &mut Context<Self>) {
        match normalize_url_with(&value, &self.search_template()) {
            Ok(url) => {
                self.url = url.clone();
                self.is_loading = true;
                self.error = None;
                self.suggestions.clear();
                self.address
                    .update(cx, |input, cx| input.set_text(url.clone(), cx));
                self.commands.send(BrowserCommand::Navigate(url));
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    // ---- historique, favoris, suggestions ----

    /// Suggestions de la barre d'adresse depuis l'historique (max 6 + ligne
    /// « Search … »). Recalculées à chaque frappe.
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        self.suggestions.clear();
        self.suggestion_sel = 0;
        if !self.prefs.history_enabled {
            cx.notify();
            return;
        }
        let Some(history) = &self.history else {
            return;
        };
        let query = self.address.read(cx).text().to_owned();
        if query.trim().is_empty() {
            return;
        }
        let now = chrono::Utc::now().timestamp_millis();
        self.suggestions = history.suggestions(&query, "Search", now);
        self.suggestion_sel = 0;
    }

    /// La page courante est-elle un favori ? (menu + étoile de la barre.)
    fn is_favorite(&self) -> bool {
        self.history
            .as_ref()
            .is_some_and(|h| h.is_favorite(&self.url))
    }

    /// Ctrl+D / menu : bascule le favori de la page courante.
    fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        if !is_web_url(&self.url) {
            return;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let title = self.title.clone();
        let url = self.url.clone();
        let added = self
            .history
            .as_mut()
            .map(|h| h.toggle_favorite(&url, &title, now))
            .unwrap_or(false);
        self.flash_status(
            if added {
                "Added to favorites"
            } else {
                "Removed from favorites"
            },
            cx,
        );
        cx.notify();
    }

    /// Enregistre la visite courante à la fin du chargement (frame
    /// principale) — les `about:` et non-http(s) sont filtrés par le store.
    fn record_visit_now(&mut self) {
        if !self.prefs.history_enabled {
            return;
        }
        let Some(history) = self.history.as_mut() else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis();
        history.record(&self.url.clone(), &self.title.clone(), now);
    }

    /// Panneau History/Favorites : même onglet → fermeture, autre → switch.
    /// Vrai si le panneau est ouvert sur l'onglet donné (réseau : re-rendu
    /// conditionnel à l'arrivée des événements).
    fn panel_visible_with(&self, tab: PanelTab) -> bool {
        self.panel.as_ref().is_some_and(|panel| panel.tab == tab)
    }

    fn toggle_panel(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        if self.panel.as_ref().is_some_and(|p| p.tab == tab) {
            self.panel = None;
        } else {
            let search = cx.new(|cx| ComposerInput::new("Search history…", cx));
            search.update(cx, |input, cx| input.set_text("", cx));
            let subscription =
                cx.subscribe(&search, |this, input, event: &ComposerInputEvent, cx| {
                    if matches!(
                        event,
                        ComposerInputEvent::Edited | ComposerInputEvent::Submitted
                    ) && let Some(panel) = this.panel.as_mut()
                    {
                        panel.query = input.read(cx).text().to_owned();
                        cx.notify();
                    }
                });
            self.panel = Some(HistoryPanel {
                tab,
                search,
                query: String::new(),
                clear_open: false,
                _search_subscription: subscription,
            });
        }
        cx.notify();
    }

    /// Action « Clear… » du panneau : suppression par portée.
    fn clear_history(&mut self, scope: history::ClearScope, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut() {
            history.clear(scope, chrono::Utc::now().timestamp_millis());
            history.flush();
        }
        if let Some(panel) = self.panel.as_mut() {
            panel.clear_open = false;
        }
        cx.notify();
    }

    /// Supprime une entrée d'historique (✕ du panneau).
    fn remove_history_entry(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut() {
            history.remove(url);
            history.flush();
        }
        cx.notify();
    }

    /// Bascule favori depuis le panneau (étoile des lignes Favorites).
    fn toggle_favorite_url(&mut self, url: &str, title: &str, cx: &mut Context<Self>) {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(history) = self.history.as_mut() {
            history.toggle_favorite(url, title, now);
        }
        cx.notify();
    }

    // --- Clear Storage ----------------------------------------------------

    /// Ouvre le dialogue « Clear Storage » (menu « … » ou Settings →
    /// « Clear browsing data… » relayé par le shell).
    pub fn open_clear_dialog(&mut self, cx: &mut Context<Self>) {
        self.clear_sel = ClearSelection::DEFAULT;
        self.clear_open = true;
        cx.notify();
    }

    /// Bascule une case du dialogue.
    fn toggle_clear_flag(&mut self, flag: ClearFlag, cx: &mut Context<Self>) {
        match flag {
            ClearFlag::Cookies => self.clear_sel.cookies = !self.clear_sel.cookies,
            ClearFlag::Cache => self.clear_sel.cache = !self.clear_sel.cache,
            ClearFlag::SiteData => self.clear_sel.site_data = !self.clear_sel.site_data,
            ClearFlag::History => self.clear_sel.history = !self.clear_sel.history,
        }
        cx.notify();
    }

    /// Origines connues pour `Storage.clearDataForOrigin` : historique +
    /// favoris + page courante (« Global » = tous les sites du profil).
    fn known_storage_origins(&self) -> Vec<String> {
        let mut urls: Vec<&str> = Vec::new();
        if is_web_url(&self.url) {
            urls.push(self.url.as_str());
        }
        if let Some(history) = &self.history {
            urls.extend(history.entries().iter().map(|e| e.url.as_str()));
            urls.extend(history.favorites().iter().map(|f| f.url.as_str()));
        }
        clear::known_origins(urls)
    }

    /// Confirmation du dialogue : envoie le ménage CDP (cookies, cache,
    /// données de sites par origine), purge l'historique si coché — les
    /// favoris survivent — et prévient le shell pour les autres surfaces.
    fn confirm_clear_storage(&mut self, cx: &mut Context<Self>) {
        let selection = self.clear_sel;
        self.clear_open = false;
        if !selection.any() {
            cx.notify();
            return;
        }
        if selection.needs_cdp() {
            let origins = self.known_storage_origins();
            self.commands
                .send(BrowserCommand::ClearBrowsingData { selection, origins });
        }
        if selection.history {
            self.clear_all_history(cx);
            cx.emit(BrowserSurfaceEvent::HistoryCleared);
        }
        self.suggestions.clear();
        self.suggestion_sel = 0;
        self.flash_status("Browsing data cleared", cx);
        cx.notify();
    }

    /// Vide tout l'historique en mémoire + disque, favoris conservés —
    /// case « History » du dialogue et broadcast du shell.
    pub fn clear_all_history(&mut self, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut() {
            history.clear(
                history::ClearScope::All,
                chrono::Utc::now().timestamp_millis(),
            );
            history.flush();
        }
        self.suggestions.clear();
        self.suggestion_sel = 0;
        if let Some(panel) = self.panel.as_mut() {
            panel.clear_open = false;
        }
        cx.notify();
    }

    /// Settings → « Clear history… » : historique ET favoris (sa méta
    /// l'annonce), fichiers tronqués par le shell en parallèle.
    pub fn clear_history_and_favorites(&mut self, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut() {
            history.clear_all();
            history.flush();
        }
        self.suggestions.clear();
        self.suggestion_sel = 0;
        if let Some(panel) = self.panel.as_mut() {
            panel.clear_open = false;
        }
        cx.notify();
    }

    /// Status line under the toolbar — auto-clears so a stale "Saved:" never
    /// sits there forever.
    fn flash_status(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status_toast = Some(message.into());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let _ = this.update(cx, |this, cx| {
                this.status_toast = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// Ctrl+L: focus the address field and select its whole content.
    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address.update(cx, |input, cx| {
            input.focus_select_all(window, cx);
        });
    }

    /// « New Tab » : retour à l'état vide. La navigation `about:blank` côté
    /// CDP détache la page courante ; les frames `about:` ne repeuplent pas
    /// `self.image`, donc l'état « Open Address… » reste affiché.
    pub fn new_tab(&mut self, cx: &mut Context<Self>) {
        self.url.clear();
        self.title.clear();
        if let Some(image) = self.image.take() {
            self.retired_images.push(image);
        }
        self.error = None;
        self.is_loading = true;
        self.can_back = false;
        self.can_forward = false;
        self.elements.clear();
        self.select_mode = false;
        self.select_hover = None;
        self.show_overlay = false;
        // Nouvel onglet : zoom par défaut, mémoire par hôte conservée.
        self.zoom_host = None;
        self.zoom = self.prefs.zoom_default.clamp(ZOOM_MIN, ZOOM_MAX);
        self.push_resize(cx);
        // Find in Page ne survit pas à un changement d'onglet.
        self.find_open = false;
        self.find_query.clear();
        self.find_count = 0;
        self.find_index = -1;
        self.find_input.update(cx, |input, cx| {
            input.set_text("", cx);
        });
        self.address.update(cx, |input, cx| {
            input.set_text("", cx);
        });
        // Page d'accueil (Settings → Browser) : `about:blank` sinon — dans
        // les deux cas l'état vide « Open Address… » reste la porte d'entrée.
        let home = self
            .prefs
            .home_url
            .as_deref()
            .filter(|url| is_web_url(url))
            .unwrap_or("about:blank");
        self.commands
            .send(BrowserCommand::Navigate(home.to_string()));
        cx.notify();
    }

    /// Bascule le mode « Add element » : curseur en croix, clics interceptés,
    /// survol sondé. Quitter efface la surbrillance et les badges numérotés.
    fn toggle_select_mode(&mut self, cx: &mut Context<Self>) {
        self.select_mode = !self.select_mode;
        if !self.select_mode {
            self.select_hover = None;
            self.show_overlay = false;
        }
        cx.notify();
    }

    fn exit_select_mode(&mut self, cx: &mut Context<Self>) {
        if self.select_mode {
            self.select_mode = false;
            self.select_hover = None;
            self.show_overlay = false;
            cx.notify();
        }
    }

    /// Clic en mode sélection : décrit l'élément sous le point. Le clic
    /// quitte le mode sauf si Shift est tenu (ajouts en rafale).
    fn describe_at(&mut self, x: f32, y: f32, keep_open: bool, cx: &mut Context<Self>) {
        self.commands.send(BrowserCommand::DescribeAt { x, y });
        if !keep_open {
            self.exit_select_mode(cx);
        }
    }

    /// `has_page` du menu : une image est affichée ET l'URL est http(s).
    fn has_page(&self) -> bool {
        self.image.is_some() && is_web_url(&self.url)
    }

    /// Items réellement rendus dans le menu « … » : toute la spec est
    /// modélisée par [`menu::all_menu_items`], mais seules les actions livrées
    /// apparaissent — chaque PR allume la sienne via [`menu::action_shipped`].
    fn menu_items(&self) -> Vec<MenuItem> {
        let favorite = self.is_favorite();
        let states = menu::menu_state(self.has_page(), self.zoom, favorite);
        menu::all_menu_items(&states, favorite)
            .into_iter()
            .filter(|item| menu::action_shipped(item.action))
            .collect()
    }

    /// Exécute une action du menu « … » ou d'un raccourci. Les variantes non
    /// encore livrées sont des no-op : leurs entrées ne sont pas rendues et
    /// leurs raccourcis sont interceptés pour ne pas partir dans Chrome.
    fn run_browser_action(
        &mut self,
        action: BrowserAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            BrowserAction::NewTab => cx.emit(BrowserSurfaceEvent::NewTab),
            BrowserAction::OpenExternalBrowser => {
                if is_web_url(&self.url) {
                    cx.open_url(&self.url);
                }
            }
            BrowserAction::BrowserSettings => cx.emit(BrowserSurfaceEvent::OpenSettings),
            BrowserAction::History => self.toggle_panel(PanelTab::History, cx),
            BrowserAction::ToggleFavorite => self.toggle_favorite(cx),
            BrowserAction::ZoomIn => self.zoom_by(1, cx),
            BrowserAction::ZoomOut => self.zoom_by(-1, cx),
            BrowserAction::ResetZoom => self.set_zoom(ZOOM_DEFAULT, cx),
            BrowserAction::FindInPage => self.open_find(window, cx),
            // Device emulation : la barre s'ouvre (ou se ferme si déjà
            // visible) ; le choix du preset se fait dans son popover.
            BrowserAction::DeviceEmulation => {
                self.emu_open = !self.emu_open;
                if self.emu_open && self.emulation.is_none() {
                    // Sans device actif, on pré-sélectionne « responsive » à
                    // la taille des champs.
                    let custom = self.emu_custom_size(cx);
                    if let Some(device) = emulation_device("responsive", custom) {
                        self.emulation = Some(device.clone());
                        self.commands.send(BrowserCommand::SetEmulation {
                            device: Some(device),
                        });
                        self.push_resize(cx);
                    }
                }
                cx.notify();
            }
            // Screenshot et Console ont quitté le menu « … » (PR 2) : la
            // capture vit dans le tiroir DevTools, la console derrière son
            // bouton dédié. Leurs entrées restent ici pour Ctrl+…/F12.
            BrowserAction::Screenshot => {
                self.commands.send(BrowserCommand::CaptureScreenshot);
            }
            BrowserAction::ClearStorage => {
                self.clear_sel = ClearSelection::DEFAULT;
                self.clear_open = true;
                cx.notify();
            }
            BrowserAction::ToggleConsole => {
                self.show_console = !self.show_console;
                cx.notify();
            }
        }
    }

    /// Ligne du menu « … » : 26 px, libellé 13 px, raccourci `text_muted`
    /// 12 px à droite, sans icône. Les entrées grisées sont inertes.
    fn browser_menu_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        item: &MenuItem,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = div()
            .id(("browser-menu-row", ix))
            .h(px(26.0))
            .w_full()
            .px(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .rounded(px(6.0))
            .text_size(px(13.0));
        if !item.enabled {
            return row
                .text_color(theme.text_muted.opacity(0.55))
                .child(item.label.clone());
        }
        let fade_key: SharedString = format!("browser-menu-row-{ix}").into();
        let action = item.action;
        let mut row = row
            .cursor_pointer()
            .text_color(motion::hover_blend(
                &fade_key,
                theme.text.opacity(0.9),
                theme.text,
            ))
            .bg(motion::hover_blend(
                &fade_key,
                crate::theme::wash(0.0),
                crate::theme::card_selected_bg(),
            ));
        row.interactivity()
            .on_hover(motion::hover_listener(fade_key.clone(), cx.entity_id()));
        row.on_click(cx.listener(move |this, _, window, cx| {
            this.dots_open = false;
            this.run_browser_action(action, window, cx);
            cx.notify();
        }))
        .child(item.label.clone())
        .when_some(item.shortcut.clone(), |el, shortcut| {
            el.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(shortcut),
            )
        })
    }

    pub fn tab_title(&self) -> SharedString {
        if !self.title.trim().is_empty() {
            return self.title.clone().into();
        }
        host_of(&self.url)
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Browser"))
    }

    fn receive_events(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        loop {
            match self.events.try_recv() {
                Ok(BrowserEvent::Page {
                    image,
                    title,
                    url,
                    session_id,
                }) => {
                    // backpressure : la frame est consommée → on acquitte
                    // Chrome pour qu'il produise la suivante
                    self.commands.send(BrowserCommand::AckFrame(session_id));
                    // Les pages `about:` (new tab) gardent l'état vide :
                    // leur frame blanche ne remplace pas « Open Address… ».
                    // URL vide = surface jamais naviguée : la frame
                    // `about:blank` de Chrome est blanche aussi.
                    if !url.is_empty()
                        && !url.starts_with("about:")
                        && let Some(previous) = self.image.replace(image)
                    {
                        self.retired_images.push(previous);
                    }
                    if !title.is_empty() {
                        self.title = title;
                    }
                    self.is_loading = false;
                    let previous_url = self.url.clone();
                    let url_changed = previous_url != url;
                    self.url = url;
                    // Changement d'hôte → zoom mémorisé par site (réglage).
                    self.apply_host_zoom(cx);
                    // La barre Find survit à la navigation : on relance la
                    // recherche sur le nouveau document (uniquement quand
                    // l'URL change — l'événement Page arrive à chaque frame).
                    if url_changed && self.find_open && !self.find_query.is_empty() {
                        self.run_find(0, cx);
                    }
                    if self.address.read(cx).text() == previous_url {
                        let address = self.url.clone();
                        self.address
                            .update(cx, |input, cx| input.set_text(address, cx));
                    }
                    self.error = None;
                    changed = true;
                }
                Ok(BrowserEvent::Title(title)) => {
                    if !title.trim().is_empty() {
                        // Titre tardif : met à jour l'entrée d'historique si
                        // la page a été enregistrée sans titre.
                        if let Some(history) = self.history.as_mut() {
                            history.record_title(&self.url.clone(), &title);
                        }
                        self.title = title;
                        changed = true;
                    }
                }
                Ok(BrowserEvent::Loading(loading)) => {
                    // l'overlay inspecteur ne recapture qu'à la fin du
                    // chargement (sinon il re-run le script à chaque frame)
                    if overlay_refresh_needed(self.is_loading, loading, self.show_overlay) {
                        self.commands.send(BrowserCommand::ToggleOverlay);
                    }
                    // Fin de chargement de la frame principale → visite.
                    if self.is_loading && !loading {
                        self.record_visit_now();
                    }
                    self.is_loading = loading;
                    changed = true;
                }
                Ok(BrowserEvent::NavHistory {
                    can_back,
                    can_forward,
                }) => {
                    if self.can_back != can_back || self.can_forward != can_forward {
                        self.can_back = can_back;
                        self.can_forward = can_forward;
                        changed = true;
                    }
                }
                Ok(BrowserEvent::Elements(elements)) => {
                    self.elements = elements;
                    changed = true;
                }
                Ok(BrowserEvent::Console(log)) => {
                    push_console(&mut self.console_logs, log);
                    changed = true;
                }
                Ok(BrowserEvent::ElementHover(hover)) => {
                    // La surbrillance n'a de sens qu'en mode sélection ; on la
                    // mémorise quand même (léger) pour ne pas désynchroniser.
                    if self.select_hover != hover {
                        self.select_hover = hover;
                        changed = true;
                    }
                }
                Ok(BrowserEvent::ElementContext(context)) => {
                    let tag = if context.tag.is_empty() {
                        "element".to_string()
                    } else {
                        context.tag.clone()
                    };
                    self.flash_status(format!("Added <{tag}> to chat"), cx);
                    cx.emit(BrowserSurfaceEvent::AddElement(context));
                    changed = true;
                }
                Ok(BrowserEvent::FindResult { count, index }) => {
                    self.find_count = count;
                    self.find_index = index;
                    changed = true;
                }
                Ok(BrowserEvent::Screenshot(bytes)) => {
                    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let file_name = format!("screenshot-{ts}.png");
                    // Dossier des captures (Settings → Browser) ; défaut :
                    // le dossier Téléchargements.
                    let dir = self
                        .prefs
                        .capture_dir
                        .as_deref()
                        .map(PathBuf::from)
                        .or_else(dirs::download_dir)
                        .unwrap_or_else(|| PathBuf::from("."));
                    let path = dir.join(&file_name);
                    if let Ok(()) = std::fs::write(&path, &bytes) {
                        self.flash_status(format!("Saved: {}", path.display()), cx);
                    } else {
                        self.flash_status(
                            format!("Screenshot captured ({} bytes)", bytes.len()),
                            cx,
                        );
                    }
                    changed = true;
                }
                Ok(BrowserEvent::DownloadStart { id, filename, url }) => {
                    downloads::apply_event(
                        &mut self.downloads,
                        &id,
                        &filename,
                        &url,
                        None,
                        None,
                        None,
                        None,
                    );
                    self.flash_status(format!("Downloading {filename}…"), cx);
                    changed = true;
                }
                Ok(BrowserEvent::DownloadProgress {
                    id,
                    received,
                    total,
                }) => {
                    downloads::apply_event(
                        &mut self.downloads,
                        &id,
                        "",
                        "",
                        None,
                        Some(received),
                        total,
                        None,
                    );
                    changed = true;
                }
                Ok(BrowserEvent::DownloadDone { id, ok, path }) => {
                    let state = if ok {
                        downloads::DownloadState::Completed
                    } else {
                        downloads::DownloadState::Canceled
                    };
                    downloads::apply_event(
                        &mut self.downloads,
                        &id,
                        "",
                        "",
                        Some(state),
                        None,
                        None,
                        path.as_deref().map(Path::new),
                    );
                    let name = self
                        .downloads
                        .iter()
                        .find(|item| item.id == id)
                        .map(|item| item.filename.clone())
                        .unwrap_or_default();
                    self.flash_status(
                        if ok {
                            format!("Download complete: {name}")
                        } else {
                            format!("Download canceled: {name}")
                        },
                        cx,
                    );
                    changed = true;
                }
                Ok(BrowserEvent::NetworkStart { id, url, method }) => {
                    self.network.started(&id, &url, &method);
                    // Re-rendu seulement si le panneau Réseau est visible :
                    // chaque requête ne doit pas réveiller toute la surface.
                    if self.panel_visible_with(PanelTab::Network) {
                        changed = true;
                    }
                }
                Ok(BrowserEvent::NetworkResponded { id, status }) => {
                    self.network.responded(&id, status);
                    if self.panel_visible_with(PanelTab::Network) {
                        changed = true;
                    }
                }
                Ok(BrowserEvent::NetworkFailed {
                    id,
                    error,
                    canceled,
                }) => {
                    self.network.failed(&id, &error, canceled);
                    if self.panel_visible_with(PanelTab::Network) {
                        changed = true;
                    }
                }
                Ok(BrowserEvent::Failed(error)) => {
                    self.error = Some(error);
                    changed = true;
                }
                Ok(BrowserEvent::Warning(warning)) => {
                    self.flash_status(warning, cx);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.error.is_none() {
                        self.error = Some("Browser engine connection terminated.".into());
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn on_page_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // Le dialogue « Clear Storage » est modal : Échap le ferme avant
        // tout le reste (select mode, raccourcis, forwarding Chrome).
        if self.clear_open && keystroke.key == "escape" {
            self.clear_open = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        // Mode « Add element » : Échap quitte, N (sans modif) affiche les
        // badges numérotés — avant tout forwarding vers Chrome.
        if self.select_mode {
            if keystroke.key == "escape"
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
            {
                self.exit_select_mode(cx);
                cx.stop_propagation();
                return;
            }
            if keystroke.key == "n"
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
            {
                self.show_overlay = !self.show_overlay;
                if self.show_overlay {
                    self.commands.send(BrowserCommand::ToggleOverlay);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
        // Raccourcis navigateur (Ctrl+T, Ctrl+F, F12…) : interceptés AVANT le
        // forwarding — sinon ils partent dans la page Chrome.
        if let Some(action) = menu::browser_shortcut(keystroke) {
            self.run_browser_action(action, window, cx);
            cx.stop_propagation();
            return;
        }
        // Le champ « Find in Page » a le focus : ses frappes (contexte
        // PaletteSearch, touches non liées) remontent jusqu'ici par
        // bubbling — elles ne doivent PAS partir dans Chrome.
        if self.find_open && self.find_input.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let modifiers = cdp_modifiers(
            keystroke.modifiers.alt,
            keystroke.modifiers.control,
            keystroke.modifiers.shift,
            keystroke.modifiers.platform,
        );

        // coller : presse-papier → Input.insertText (IME + collage fiables)
        let ctrl_or_meta = keystroke.modifiers.control || keystroke.modifiers.platform;
        if ctrl_or_meta && keystroke.key == "v" && !keystroke.modifiers.shift {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                && !text.is_empty()
            {
                self.commands.send(BrowserCommand::InsertText(text));
                cx.stop_propagation();
            }
            return;
        }

        // copier / couper / tout sélectionner : forward avec modifiers pour Chrome
        if ctrl_or_meta
            && matches!(keystroke.key.as_str(), "a" | "c" | "x")
            && !keystroke.modifiers.shift
        {
            let (key, code, vk) = match keystroke.key.as_str() {
                "a" => ("a", "KeyA", Some(65u32)),
                "c" => ("c", "KeyC", Some(67u32)),
                _ => ("x", "KeyX", Some(88u32)),
            };
            self.commands.send(BrowserCommand::KeyPress {
                key,
                code,
                vk,
                modifiers,
            });
            self.commands.send(BrowserCommand::KeyRelease {
                key,
                code,
                modifiers,
            });
            cx.stop_propagation();
            return;
        }

        if let Some((key, code, vk)) = gpui_key_to_cdp(&keystroke.key) {
            self.commands.send(BrowserCommand::KeyPress {
                key,
                code,
                vk: Some(vk),
                modifiers,
            });
            cx.stop_propagation();
            return;
        }

        if let Some(text) = keystroke
            .key_char
            .as_deref()
            .filter(|text| !text.is_empty() && !text.chars().all(char::is_control))
        {
            self.commands
                .send(BrowserCommand::TypeText(text.to_owned()));
            cx.stop_propagation();
        }
    }

    /// Drag de la poignée du tiroir DevTools : la hauteur suit le pointeur
    /// (monter = agrandir), bornée à [120, 60 % de la fenêtre].
    fn on_console_drag(
        &mut self,
        event: &gpui::DragMoveEvent<ConsoleDrawerResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((start_y, start_h)) = self.console_drag_anchor else {
            return;
        };
        let max_h = (f32::from(window.viewport_size().height) * 0.6).max(120.0);
        let dy = start_y - f32::from(event.event.position.y);
        self.console_height = (start_h + dy).clamp(120.0, max_h);
        cx.notify();
    }

    fn on_page_key_up(
        &mut self,
        event: &gpui::KeyUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = cdp_modifiers(
            keystroke.modifiers.alt,
            keystroke.modifiers.control,
            keystroke.modifiers.shift,
            keystroke.modifiers.platform,
        );
        if let Some((key, code, _vk)) = gpui_key_to_cdp(&keystroke.key) {
            self.commands.send(BrowserCommand::KeyRelease {
                key,
                code,
                modifiers,
            });
            cx.stop_propagation();
        }
    }
}

impl Drop for BrowserSurface {
    fn drop(&mut self) {
        self.commands.send(BrowserCommand::Shutdown);
        if let Some(history) = self.history.as_mut() {
            history.flush();
        }
    }
}

impl BrowserSurface {
    /// Dropdown des suggestions sous la barre d'adresse : lignes de 28 px,
    /// titre + hôte, sélection clavier surlignée, clic = naviguer.
    fn render_suggestions(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut list = div()
            .id("browser-suggestions-list")
            .absolute()
            .top(px(30.0))
            .left_0()
            .right_0()
            .max_h(px(240.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .shadow_lg();
        for (ix, suggestion) in self.suggestions.iter().enumerate() {
            let selected = ix == self.suggestion_sel;
            let url = suggestion.target.clone();
            let group: SharedString = format!("browser-suggestion-{ix}").into();
            let icon_path = if suggestion.is_search {
                icons::SEARCH
            } else if self
                .history
                .as_ref()
                .is_some_and(|h| h.is_favorite(&suggestion.target))
            {
                icons::STAR_BOLD
            } else {
                icons::GLOBAL
            };
            let mut row = div()
                .id(("browser-suggestion", ix))
                .group(group)
                .h(px(28.0))
                .w_full()
                .px(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .text_size(px(12.5))
                .when(selected, |el| el.bg(crate::theme::card_selected_bg()))
                .hover(|s| s.bg(crate::theme::ink(0.05)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.navigate_to(url.clone(), cx);
                }))
                .child(
                    crate::icons::icon(icon_path)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(theme.text)
                        .child(suggestion.title.clone()),
                );
            if let Some(host) = &suggestion.host {
                row = row.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(host.clone()),
                );
            }
            list = list.child(row);
        }
        list
    }

    /// Panneau History/Favorites : remplace la zone de page quand ouvert.
    /// En-tête avec onglets + recherche + « Clear… », liste groupée par jour.
    fn render_panel(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.panel.as_ref()?;
        let history = self.history.as_ref()?;
        let now = chrono::Utc::now().timestamp_millis();
        let query = panel.query.trim().to_lowercase();
        let matches = |title: &str, url: &str| {
            query.is_empty()
                || title.to_lowercase().contains(&query)
                || url.to_lowercase().contains(&query)
        };

        let mut body = div()
            .id("browser-panel-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();

        match panel.tab {
            PanelTab::History => {
                let entries: Vec<history::HistoryEntry> = history
                    .entries()
                    .iter()
                    .filter(|e| matches(&e.title, &e.url))
                    .cloned()
                    .collect();
                if entries.is_empty() {
                    body = body.child(self.panel_empty(theme, "No history yet"));
                } else {
                    for (group, items) in history::group_by_day(&entries, now) {
                        let label = match group {
                            history::DayGroup::Today => "Today".to_string(),
                            history::DayGroup::Yesterday => "Yesterday".to_string(),
                            history::DayGroup::Date(d) => d,
                        };
                        body = body.child(
                            div()
                                .px(px(12.0))
                                .pt(px(10.0))
                                .pb(px(2.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(label),
                        );
                        for (ix, entry) in items.iter().enumerate() {
                            body = body.child(self.panel_history_row(theme, ix, entry, cx));
                        }
                    }
                }
            }
            PanelTab::Favorites => {
                let favorites: Vec<&history::Favorite> = history
                    .favorites()
                    .iter()
                    .filter(|f| matches(&f.title, &f.url))
                    .collect();
                if favorites.is_empty() {
                    body = body.child(self.panel_empty(theme, "No favorites yet"));
                } else {
                    for (ix, fav) in favorites.iter().enumerate() {
                        body = body.child(self.panel_favorite_row(theme, ix, fav, cx));
                    }
                }
            }
            PanelTab::Downloads => {
                if self.downloads.is_empty() {
                    body = body.child(self.panel_empty(theme, "No downloads yet"));
                } else {
                    for (ix, item) in self.downloads.iter().enumerate().rev() {
                        body = body.child(self.panel_download_row(theme, ix, item, cx));
                    }
                }
            }
            PanelTab::Network => {
                if self.network.is_empty() {
                    body = body.child(self.panel_empty(theme, "No requests yet"));
                } else {
                    for (ix, entry) in self.network.entries().iter().enumerate().rev() {
                        body = body.child(self.panel_network_row(theme, ix, entry, cx));
                    }
                }
            }
        }

        let tab_history_active = panel.tab == PanelTab::History;
        let tab_fav_active = panel.tab == PanelTab::Favorites;
        let tab_dl_active = panel.tab == PanelTab::Downloads;
        let tab_net_active = panel.tab == PanelTab::Network;
        let search = panel.search.clone();
        let history_like = tab_history_active || tab_fav_active;
        let clear_open = panel.clear_open;

        Some(
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.surface)
                // En-tête : onglets + recherche + Clear…
                .child(
                    div()
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(10.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(self.panel_tab_button(
                            theme,
                            "browser-panel-tab-history",
                            "History",
                            tab_history_active,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_panel(PanelTab::History, cx);
                            }),
                        ))
                        .child(self.panel_tab_button(
                            theme,
                            "browser-panel-tab-favorites",
                            "Favorites",
                            tab_fav_active,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_panel(PanelTab::Favorites, cx);
                            }),
                        ))
                        .child(self.panel_tab_button(
                            theme,
                            "browser-panel-tab-downloads",
                            "Downloads",
                            tab_dl_active,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_panel(PanelTab::Downloads, cx);
                            }),
                        ))
                        .child(self.panel_tab_button(
                            theme,
                            "browser-panel-tab-network",
                            "Network",
                            tab_net_active,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_panel(PanelTab::Network, cx);
                            }),
                        ))
                        .child(div().flex_1())
                        .when(history_like, |el| {
                            el.child(
                                div()
                                    .id("browser-panel-search")
                                    .w(px(200.0))
                                    .h(px(24.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(crate::theme::ink(0.04))
                                    .px(px(6.0))
                                    .flex()
                                    .items_center()
                                    .child(search),
                            )
                        })
                        .when(panel.tab == PanelTab::History, |el| {
                            el.child(
                                div()
                                    .id("browser-panel-clear")
                                    .relative()
                                    .h(px(24.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(panel) = this.panel.as_mut() {
                                            panel.clear_open = !panel.clear_open;
                                        }
                                        cx.notify();
                                    }))
                                    .child("Clear…")
                                    .when(clear_open, |el| {
                                        el.child(
                                            div()
                                                .absolute()
                                                .top(px(28.0))
                                                .right_0()
                                                .w(px(160.0))
                                                .flex()
                                                .flex_col()
                                                .rounded(px(8.0))
                                                .border_1()
                                                .border_color(theme.border)
                                                .bg(theme.surface)
                                                .shadow_lg()
                                                .child(self.panel_clear_row(
                                                    theme,
                                                    0,
                                                    "Last hour",
                                                    history::ClearScope::Hour,
                                                    cx,
                                                ))
                                                .child(self.panel_clear_row(
                                                    theme,
                                                    1,
                                                    "Today",
                                                    history::ClearScope::Today,
                                                    cx,
                                                ))
                                                .child(self.panel_clear_row(
                                                    theme,
                                                    2,
                                                    "All history",
                                                    history::ClearScope::All,
                                                    cx,
                                                )),
                                        )
                                    }),
                            )
                        })
                        .when(panel.tab == PanelTab::Network, |el| {
                            el.child(
                                div()
                                    .id("browser-panel-clear-network")
                                    .h(px(24.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.network.clear();
                                        cx.notify();
                                    }))
                                    .child("Clear"),
                            )
                        })
                        .when(panel.tab == PanelTab::Downloads, |el| {
                            el.child(
                                div()
                                    .id("browser-panel-clear-downloads")
                                    .h(px(24.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        // Les items en cours restent suivis ;
                                        // on purge seulement l'historique.
                                        this.downloads.retain(|item| !item.is_terminal());
                                        cx.notify();
                                    }))
                                    .child("Clear finished"),
                            )
                        }),
                )
                .child(body)
                .into_any_element(),
        )
    }

    /// Ligne du panneau Downloads : nom, progression (barre + %), état ;
    /// terminé → « Open » (application par défaut) et « Reveal » (dossier).
    fn panel_download_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        item: &downloads::DownloadItem,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use downloads::DownloadState;
        let state_label = match item.state {
            DownloadState::InProgress => "Downloading…".to_string(),
            DownloadState::Completed => "Completed".to_string(),
            DownloadState::Canceled => "Canceled".to_string(),
        };
        let percent = item
            .progress()
            .map(|p| format!("{:.0}%", p * 100.0))
            .unwrap_or_default();
        let from = host_of(&item.url).unwrap_or_default();
        let open_path = item.path.clone();
        let reveal_path = item.path.clone();
        div()
            .id(("browser-panel-download", ix))
            .h(px(44.0))
            .w_full()
            .px(px(12.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(item.filename.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(if percent.is_empty() {
                                state_label.clone()
                            } else {
                                format!("{state_label} · {percent}")
                            }),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        // barre de progression (0 quand la taille est inconnue)
                        div()
                            .flex_1()
                            .h(px(3.0))
                            .rounded(px(2.0))
                            .bg(crate::theme::ink(0.08))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(
                                        item.progress().unwrap_or(0.0).clamp(0.0, 1.0),
                                    ))
                                    .rounded(px(2.0))
                                    .bg(theme.accent),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(160.0))
                            .overflow_hidden()
                            .text_size(px(10.5))
                            .text_color(theme.text_muted)
                            .child(from),
                    )
                    .when(item.state == DownloadState::Completed, |el| {
                        el.child(
                            div()
                                .id(("browser-panel-download-open", ix))
                                .flex_none()
                                .px(px(6.0))
                                .h(px(18.0))
                                .flex()
                                .items_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .text_size(px(11.0))
                                .text_color(theme.text)
                                .hover(|s| s.bg(crate::theme::ink(0.10)))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if let Some(path) = &open_path {
                                        cx.open_with_system(path);
                                    }
                                }))
                                .child("Open"),
                        )
                        .child(
                            div()
                                .id(("browser-panel-download-reveal", ix))
                                .flex_none()
                                .px(px(6.0))
                                .h(px(18.0))
                                .flex()
                                .items_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .hover(|s| s.bg(crate::theme::ink(0.10)))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    // gpui révèle le fichier dans le
                                    // gestionnaire de fichiers de l'OS
                                    if let Some(path) = reveal_path.as_ref() {
                                        cx.reveal_path(path);
                                    }
                                }))
                                .child("Reveal"),
                        )
                    }),
            )
    }

    /// Ligne du panneau Network : méthode, URL, statut/erreur. Erreurs en
    /// rouge, certificats signalés, hôte local marqué, HTTP non chiffré
    /// distingué d'HTTPS.
    fn panel_network_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        entry: &network::NetworkEntry,
        _cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let scheme_https = entry.url.starts_with("https://");
        let local = network::is_local_host(&entry.url);
        let cert = entry.cert_error();
        let status_color = if cert || entry.failed() {
            theme.danger
        } else {
            theme.text_muted
        };
        let scheme_label = if local {
            "local".to_string()
        } else if scheme_https {
            "https".to_string()
        } else if entry.url.starts_with("http://") {
            "http".to_string()
        } else {
            String::new()
        };
        let scheme_color = if cert || entry.failed() {
            theme.danger
        } else if local || scheme_https {
            theme.success
        } else {
            theme.warning
        };
        div()
            .id(("browser-panel-network", ix))
            .h(px(30.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_none()
                    .w(px(44.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(entry.method.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(11.5))
                    .text_color(theme.text)
                    .child(entry.url.clone()),
            )
            .when(!scheme_label.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.5))
                        .text_color(scheme_color)
                        .child(scheme_label),
                )
            })
            .child(
                div()
                    .flex_none()
                    .max_w(px(170.0))
                    .overflow_hidden()
                    .text_size(px(10.5))
                    .text_color(status_color)
                    .child(entry.status_label()),
            )
    }

    fn panel_empty(&self, theme: &crate::theme::Theme, label: &'static str) -> gpui::Div {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(theme.text_muted)
            .child(label)
    }

    fn panel_tab_button(
        &self,
        theme: &crate::theme::Theme,
        id: &'static str,
        label: &'static str,
        active: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(24.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .when(active, |el| {
                el.bg(crate::theme::card_selected_bg())
                    .text_color(theme.text)
            })
            .when(!active, |el| el.text_color(theme.text_muted))
            .hover(|s| s.bg(crate::theme::ink(0.06)))
            .on_click(on_click)
            .child(label)
    }

    fn panel_clear_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        label: &'static str,
        scope: history::ClearScope,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(("browser-panel-clear", ix))
            .h(px(26.0))
            .w_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(theme.text)
            .hover(|s| s.bg(crate::theme::ink(0.06)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.clear_history(scope, cx);
            }))
            .child(label)
    }

    /// Ligne du panneau History : titre, hôte, heure ; clic = naviguer,
    /// ✕ au survol = supprimer.
    fn panel_history_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        entry: &history::HistoryEntry,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let url = entry.url.clone();
        let title = if entry.title.trim().is_empty() {
            entry.url.clone()
        } else {
            entry.title.clone()
        };
        let host = history_host(&entry.url).unwrap_or_default();
        let time = format_time(entry.last_visit);
        let remove_url = entry.url.clone();
        let group: SharedString = format!("browser-panel-row-{ix}").into();
        div()
            .id(("browser-panel-history", ix))
            .group(group.clone())
            .relative()
            .h(px(30.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.04)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(url.clone(), cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(
                        div()
                            .overflow_hidden()
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(host),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(time),
            )
            // ✕ suppression au survol.
            .child(
                div()
                    .id(("browser-panel-remove", ix))
                    .absolute()
                    .right(px(8.0))
                    .top_0()
                    .h_full()
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .opacity(0.0)
                    .group_hover(group, |s| s.opacity(1.0))
                    .hover(|s| s.bg(crate::theme::ink(0.10)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_history_entry(&remove_url, cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(10.0))
                            .text_color(theme.text_muted),
                    ),
            )
    }

    /// Ligne du panneau Favorites : ★, titre, hôte ; clic = naviguer,
    /// ✕ au survol = retirer des favoris.
    fn panel_favorite_row(
        &self,
        theme: &crate::theme::Theme,
        ix: usize,
        fav: &history::Favorite,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let url = fav.url.clone();
        let title = if fav.title.trim().is_empty() {
            fav.url.clone()
        } else {
            fav.title.clone()
        };
        let host = history_host(&fav.url).unwrap_or_default();
        let unlike_url = fav.url.clone();
        let unlike_title = fav.title.clone();
        let group: SharedString = format!("browser-panel-fav-{ix}").into();
        div()
            .id(("browser-panel-favorite", ix))
            .group(group.clone())
            .relative()
            .h(px(30.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.04)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(url.clone(), cx);
            }))
            .child(
                crate::icons::icon(icons::STAR_BOLD)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.warning),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(host),
            )
            .child(
                div()
                    .id(("browser-panel-unfavorite", ix))
                    .absolute()
                    .right(px(8.0))
                    .top_0()
                    .h_full()
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .opacity(0.0)
                    .group_hover(group, |s| s.opacity(1.0))
                    .hover(|s| s.bg(crate::theme::ink(0.10)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_favorite_url(&unlike_url, &unlike_title, cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(10.0))
                            .text_color(theme.text_muted),
                    ),
            )
    }

    /// Dialogue modal « Clear Storage » : 4 cases (cookies + cache cochées
    /// par défaut), Cancel / Clear. « Global » = tous les sites du profil.
    fn render_clear_dialog(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sel = self.clear_sel;
        let flag_row = |flag: ClearFlag,
                        label: &'static str,
                        meta: &'static str,
                        checked: bool|
         -> gpui::Stateful<gpui::Div> {
            div()
                .id(("browser-clear-flag", flag as usize))
                .h(px(34.0))
                .w_full()
                .px(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.05)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_clear_flag(flag, cx);
                }))
                .child(
                    div()
                        .size(px(14.0))
                        .flex_none()
                        .rounded(px(3.0))
                        .border_1()
                        .border_color(if checked {
                            theme.accent
                        } else {
                            theme.border_strong
                        })
                        .bg(if checked {
                            theme.accent
                        } else {
                            crate::theme::ink(0.0)
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(checked, |el| {
                            el.child(
                                crate::icons::icon(crate::icons::CHECK)
                                    .size(px(10.0))
                                    .text_color(theme.bg),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(theme.text)
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(meta),
                        ),
                )
        };
        let can_clear = sel.any();
        let confirm_entity = cx.entity().clone();
        div()
            .id("browser-clear-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgb(0x000000).opacity(0.35))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                div()
                    .w(px(340.0))
                    .flex()
                    .flex_col()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .px(px(14.0))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .child("Clear browsing data"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(
                                        "Choose what to remove. Applies to every site in this \
                                         browser profile.",
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(4.0))
                            .flex()
                            .flex_col()
                            .child(flag_row(
                                ClearFlag::Cookies,
                                "Cookies and sign-in data",
                                "Sessions and trackers stored by sites",
                                sel.cookies,
                            ))
                            .child(flag_row(
                                ClearFlag::Cache,
                                "Cached files",
                                "Images, scripts and pages kept for speed",
                                sel.cache,
                            ))
                            .child(flag_row(
                                ClearFlag::SiteData,
                                "Site data",
                                "localStorage, IndexedDB and service workers",
                                sel.site_data,
                            ))
                            .child(flag_row(
                                ClearFlag::History,
                                "Browsing history",
                                "Visited pages — favorites are kept",
                                sel.history,
                            )),
                    )
                    .child(
                        div()
                            .px(px(14.0))
                            .py(px(10.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("browser-clear-cancel")
                                    .h(px(26.0))
                                    .px(px(10.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(theme.text)
                                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.clear_open = false;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("browser-clear-confirm")
                                    .h(px(26.0))
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(gpui::white())
                                    .bg(if can_clear {
                                        theme.danger
                                    } else {
                                        theme.text_muted.opacity(0.4)
                                    })
                                    .when(!can_clear, |el| el.cursor_default())
                                    .hover(|s| if can_clear { s.opacity(0.9) } else { s })
                                    .on_click(move |_, _, cx| {
                                        confirm_entity.update(cx, |this, cx| {
                                            this.confirm_clear_storage(cx);
                                        });
                                    })
                                    .child("Clear"),
                            ),
                    ),
            )
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired_images.drain(..) {
            gpui::ImageSource::Render(image).evict(Some(window), cx);
        }

        let theme = crate::theme::Theme::of(cx).clone();
        let address_focused = {
            let input = self.address.read(cx);
            input.focus_handle(cx).is_focused(window)
        };
        let entity_empty = cx.entity().clone();
        // Position de la souris vue par le panneau ; press et release sont des
        // commandes séparées (drag + sélection de texte), moves = hover/menus.
        // Les handlers gauche/déplacement passent par `cx.listener` pour lire
        // `select_mode` ; le clic droit reste une closure simple.
        let press_commands = self.commands.clone();
        let release_commands = self.commands.clone();
        let scroll_commands = self.commands.clone();
        let press_bounds = self.page_bounds.clone();
        let release_bounds = self.page_bounds.clone();
        let scroll_bounds = self.page_bounds.clone();
        let page_focus = self.page_focus.clone();
        let image = self.image.clone();
        // Transform écran→CSS pour les closures sans `this` (bouton droit,
        // molette) — les `cx.listener` appellent `this.view_transform()`.
        // `Copy` : capturé par les closures.
        let view = self.view_transform();
        // État vide : 5 dernières visites + favoris pour la section Recent.
        let recent_entries = self
            .history
            .as_ref()
            .map(|h| h.recent(5))
            .unwrap_or_default();
        let favorite_entries: Vec<(String, String)> = self
            .history
            .as_ref()
            .map(|h| {
                h.favorites()
                    .iter()
                    .take(5)
                    .map(|f| (f.url.clone(), f.title.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let page_view = image
            .map(|image| {
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                if self.is_loading {
                    return div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.text_muted)
                        .text_size(px(13.0))
                        .child("Loading…")
                        .into_any_element();
                }
                // Empty state: fake-field row (click → address bar focused)
                // + section « Recent » : 5 dernières visites et favoris.
                let caps: [SharedString; 3] = if cfg!(target_os = "macos") {
                    ["⌘".into(), "+".into(), "L".into()]
                } else {
                    ["Ctrl".into(), "+".into(), "L".into()]
                };
                let entity_recent = entity_empty.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(72.0))
                    .child(
                        div()
                            .id("browser-empty-address")
                            .h(px(28.0))
                            .w_full()
                            .max_w(px(320.0))
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(8.0))
                            .cursor_pointer()
                            .when(address_focused, |el| el.shadow(theme.mark_ring()))
                            .hover(|s| s.bg(crate::theme::ink(0.04)))
                            .on_click(move |_event, window, cx| {
                                entity_empty.update(cx, |this, cx| {
                                    this.focus_address(window, cx);
                                });
                            })
                            .child(
                                crate::icons::icon(icons::GLOBAL)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.5))
                                    .text_color(theme.text)
                                    .child("Open Address…"),
                            )
                            .children(caps.iter().enumerate().map(|(ix, cap)| {
                                div()
                                    .h(px(18.0))
                                    .min_w(px(18.0))
                                    .px(px(5.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .bg(crate::theme::ink(0.06))
                                    .text_size(px(11.0))
                                    .text_color(if ix == 1 {
                                        theme.text_faint
                                    } else {
                                        theme.text_muted
                                    })
                                    .child(cap.clone())
                            })),
                    )
                    // Section « Recent » : 5 dernières visites puis les
                    // favoris — clic = naviguer dans cet onglet.
                    .when(
                        !recent_entries.is_empty() || !favorite_entries.is_empty(),
                        |el| {
                            el.child(
                                div()
                                    .mt(px(28.0))
                                    .w_full()
                                    .max_w(px(360.0))
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .px(px(8.0))
                                            .pb(px(4.0))
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted)
                                            .child("Recent"),
                                    )
                                    .children(recent_entries.iter().enumerate().map(
                                        |(ix, entry)| {
                                            let url = entry.url.clone();
                                            let title = if entry.title.trim().is_empty() {
                                                entry.url.clone()
                                            } else {
                                                entry.title.clone()
                                            };
                                            let host = history_host(&entry.url).unwrap_or_default();
                                            div()
                                                .id(("browser-recent", ix))
                                                .h(px(28.0))
                                                .w_full()
                                                .px(px(8.0))
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap(px(8.0))
                                                .rounded(px(6.0))
                                                .cursor_pointer()
                                                .hover(|s| s.bg(crate::theme::ink(0.04)))
                                                .on_click({
                                                    let entity = entity_recent.clone();
                                                    move |_, _, cx| {
                                                        entity.update(cx, |this, cx| {
                                                            this.navigate_to(url.clone(), cx);
                                                        });
                                                    }
                                                })
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .text_size(px(12.5))
                                                        .text_color(theme.text)
                                                        .child(title),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .text_size(px(11.0))
                                                        .text_color(theme.text_muted)
                                                        .child(host),
                                                )
                                        },
                                    ))
                                    .children(favorite_entries.iter().enumerate().map(
                                        |(ix, (url, title))| {
                                            let title = if title.trim().is_empty() {
                                                url.clone()
                                            } else {
                                                title.clone()
                                            };
                                            let host = history_host(url).unwrap_or_default();
                                            div()
                                                .id(("browser-recent-fav", ix))
                                                .h(px(28.0))
                                                .w_full()
                                                .px(px(8.0))
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap(px(8.0))
                                                .rounded(px(6.0))
                                                .cursor_pointer()
                                                .hover(|s| s.bg(crate::theme::ink(0.04)))
                                                .on_click({
                                                    let entity = entity_recent.clone();
                                                    let target = url.clone();
                                                    move |_, _, cx| {
                                                        entity.update(cx, |this, cx| {
                                                            this.navigate_to(target.clone(), cx);
                                                        });
                                                    }
                                                })
                                                .child(
                                                    crate::icons::icon(icons::STAR_BOLD)
                                                        .size(px(11.0))
                                                        .flex_none()
                                                        .text_color(theme.warning),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .text_size(px(12.5))
                                                        .text_color(theme.text)
                                                        .child(title),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .text_size(px(11.0))
                                                        .text_color(theme.text_muted)
                                                        .child(host),
                                                )
                                        },
                                    )),
                            )
                        },
                    )
                    .into_any_element()
            });

        // Badges numérotés du mode « Add element » (touche N) : chaque badge
        // décrit l'élément à son centre — les coordonnées overlay sont les
        // mêmes pixels CSS que ceux envoyés à Chrome.
        let overlay_elements = if self.select_mode && self.show_overlay && !self.elements.is_empty()
        {
            let elements = self.elements.clone();
            let surface_weak = cx.entity().downgrade();
            div()
                .absolute()
                .inset_0()
                .children(elements.into_iter().map(|el| {
                    // Rects en pixels CSS → position écran dans le cadre
                    // (× zoom × fit = view.div).
                    let z = view.div;
                    let (x, y) = (el.x * z, el.y * z);
                    let label = format!("#{} {}", el.uid, el.text);
                    let center_x = el.x + el.width / 2.0;
                    let center_y = el.y + el.height / 2.0;
                    let surface_weak = surface_weak.clone();
                    div()
                        .id(("browser-element-badge", el.uid as usize))
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .px(px(4.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.solid)
                        .text_color(theme.on_solid)
                        .text_size(px(10.0))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            // Même sortie de mode qu'un clic normal sur la page.
                            let _ = surface_weak.update(cx, |this, cx| {
                                this.describe_at(center_x, center_y, false, cx);
                            });
                        })
                        .child(label)
                }))
                .into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };

        // Surbrillance de survol (mode sélection) : contour + pastille
        // « tag#id · L×H » au-dessus de l'élément sondé.
        let hover_badge = if self.select_mode
            && let Some(hover) = &self.select_hover
            && hover.width > 0.0
            && hover.height > 0.0
        {
            // Rect du survol en pixels CSS → écran × (zoom × fit).
            let z = view.div;
            let (hx, hy, hw, hh) = (hover.x * z, hover.y * z, hover.width * z, hover.height * z);
            let badge_top = (hy - 18.0).max(0.0);
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .absolute()
                        .left(px(hx))
                        .top(px(hy))
                        .w(px(hw))
                        .h(px(hh))
                        .border_2()
                        .border_color(theme.accent)
                        .bg(theme.accent.opacity(0.08)),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(hx))
                        .top(px(badge_top))
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(theme.solid)
                        .text_color(theme.on_solid)
                        .text_size(px(10.0))
                        .child(hover.label.clone()),
                )
                .into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };

        // Barre « Find in Page » : flottante en haut à droite de la page,
        // champ + compteur « 3/17 » + ↑ ↓ + ✕. Les touches ⏎/⇧⏎/Échap
        // remontent du champ (PaletteSearch) à `on_find_key_down`.
        let find_bar = if self.find_open {
            let find_prev = cx.entity().clone();
            let find_next = cx.entity().clone();
            let find_close = cx.entity().clone();
            let counter = if self.find_count > 0 && self.find_index >= 0 {
                format!("{}/{}", self.find_index + 1, self.find_count)
            } else {
                "0/0".to_string()
            };
            let nav_button = |icon: &'static str,
                              entity: Entity<BrowserSurface>,
                              step: i32,
                              theme: &crate::theme::Theme|
             -> gpui::Stateful<gpui::Div> {
                div()
                    .id(("browser-find-nav", usize::from(step > 0)))
                    .size(px(22.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.08)))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if !this.find_query.is_empty() {
                                this.run_find(step, cx);
                            }
                        });
                    })
                    .child(
                        icons::icon(icon)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
            };
            div()
                .id("browser-find-bar")
                .absolute()
                .top(px(8.0))
                .right(px(8.0))
                .h(px(30.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .pl(px(8.0))
                .pr(px(4.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .shadow_md()
                .on_key_down(cx.listener(Self::on_find_key_down))
                .child(div().w(px(180.0)).child(self.find_input.clone()))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(if self.find_count > 0 {
                            theme.text_muted
                        } else {
                            theme.danger
                        })
                        .child(counter),
                )
                .child(nav_button(icons::ARROW_UP, find_prev, 1, &theme))
                .child(nav_button(icons::ARROW_DOWN, find_next, -1, &theme))
                .child(
                    div()
                        .id("browser-find-close")
                        .size(px(22.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ink(0.08)))
                        .on_click(move |_, window, cx| {
                            find_close.update(cx, |this, cx| {
                                this.close_find(window, cx);
                            });
                        })
                        .child(
                            icons::icon(icons::CLOSE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };

        let page = BrowserViewport {
            child: div()
                .id("komet-browser-page")
                .size_full()
                .relative()
                .track_focus(&self.page_focus)
                .on_key_down(cx.listener(Self::on_page_key_down))
                .on_key_up(cx.listener(Self::on_page_key_up))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        window.focus(&this.page_focus, cx);
                        let Some((x, y)) =
                            localize_in(&this.page_bounds, &event.position, this.view_transform())
                        else {
                            return;
                        };
                        // Mode sélection : le clic ne part pas dans la page ;
                        // on décrit l'élément sous le point (Shift = rester).
                        if this.select_mode {
                            cx.stop_propagation();
                            this.describe_at(x, y, event.modifiers.shift, cx);
                            return;
                        }
                        this.commands.send(BrowserCommand::MousePress {
                            x,
                            y,
                            button: "left",
                            count: event.click_count as u32,
                        });
                    }),
                )
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    window.focus(&page_focus, cx);
                    if let Some((x, y)) = localize_in(&press_bounds, &event.position, view) {
                        press_commands.send(BrowserCommand::MousePress {
                            x,
                            y,
                            button: "right",
                            count: event.click_count as u32,
                        });
                    }
                })
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseUpEvent, _window, cx| {
                        // En mode sélection aucun press n'est parti : le
                        // release correspondant ne doit pas partir non plus.
                        if this.select_mode {
                            cx.stop_propagation();
                            return;
                        }
                        if let Some((x, y)) =
                            localize_in(&this.page_bounds, &event.position, this.view_transform())
                        {
                            this.commands.send(BrowserCommand::MouseRelease {
                                x,
                                y,
                                button: "left",
                                count: event.click_count as u32,
                            });
                        }
                    }),
                )
                .on_mouse_up(MouseButton::Right, move |event, _window, _cx| {
                    if let Some((x, y)) = localize_in(&release_bounds, &event.position, view) {
                        release_commands.send(BrowserCommand::MouseRelease {
                            x,
                            y,
                            button: "right",
                            count: event.click_count as u32,
                        });
                    }
                })
                .on_mouse_move(
                    cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                        let Some((x, y)) =
                            localize_in(&this.page_bounds, &event.position, this.view_transform())
                        else {
                            return;
                        };
                        // Mode sélection : sonde throttlée (~25/s) à la place du
                        // simple MouseMove — la page ne voit plus le survol.
                        if this.select_mode {
                            if this.last_probe.get().elapsed() >= Duration::from_millis(40) {
                                this.last_probe.set(Instant::now());
                                this.commands.send(BrowserCommand::ProbeAt { x, y });
                            }
                            cx.stop_propagation();
                            return;
                        }
                        this.commands.send(BrowserCommand::MouseMove { x, y });
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(move |event: &ScrollWheelEvent, _, cx| {
                    // Position et deltas en pixels CSS (÷ zoom × fit, et
                    // rejetés hors du cadre émulé).
                    let div = view.div.max(0.05);
                    let (delta_x, delta_y) = match event.delta {
                        ScrollDelta::Pixels(delta) => (
                            f64::from(delta.x) / div as f64,
                            f64::from(delta.y) / div as f64,
                        ),
                        ScrollDelta::Lines(delta) => (
                            f64::from(delta.x) * 40.0 / div as f64,
                            f64::from(delta.y) * 40.0 / div as f64,
                        ),
                    };
                    let Some((cx_f, cy_f)) = localize_in(&scroll_bounds, &event.position, view)
                    else {
                        cx.stop_propagation();
                        return;
                    };
                    scroll_commands.send(BrowserCommand::Scroll {
                        x: cx_f,
                        y: cy_f,
                        delta_x,
                        delta_y,
                    });
                    cx.stop_propagation();
                })
                // Device emulation : la page (image + overlays + find) est
                // centrée dans un cadre à l'échelle `fit` ; sans émulation le
                // contenu remplit la zone.
                .child({
                    let content = div()
                        .absolute()
                        .inset_0()
                        .child(page_view)
                        .child(overlay_elements)
                        .child(hover_badge)
                        .child(find_bar);
                    if self.emulation.is_some() {
                        div()
                            .absolute()
                            .left(px(view.ox))
                            .top(px(view.oy))
                            .w(px(view.fw))
                            .h(px(view.fh))
                            .border_1()
                            .border_color(theme.border_strong)
                            .rounded(px(8.0))
                            .overflow_hidden()
                            .bg(theme.bg)
                            .shadow_lg()
                            .child(content)
                    } else {
                        content
                    }
                })
                .when(self.select_mode, |el| {
                    el.cursor(gpui::CursorStyle::Crosshair)
                })
                .into_any_element(),
            bounds: self.page_bounds.clone(),
        };

        // Check if viewport size changed and notify backend — débouncé :
        // on ne redémarre pas le screencast à chaque étape d'un drag.
        if let Some(bounds) = self.page_bounds.get() {
            let w = f32::from(bounds.size.width) as u32;
            let h = f32::from(bounds.size.height) as u32;
            if w > 100 && h > 100 {
                let prev = self.last_viewport.get();
                // Trigger on first valid bounds or when size changes by more than 8px
                if prev == (0, 0) || w.abs_diff(prev.0) > 8 || h.abs_diff(prev.1) > 8 {
                    self.last_viewport.set((w, h));
                    self.last_scale.set(window.scale_factor());
                    self.pending_resize.set(Some((w, h, window.scale_factor())));
                    let generation = self.resize_gen.get() + 1;
                    self.resize_gen.set(generation);
                    if !self.resize_scheduled.replace(true) {
                        self.schedule_resize_flush(cx);
                    }
                }
            }
        }

        let back = self.commands.clone();
        let forward = self.commands.clone();
        let reload = self.commands.clone();
        let retry = self.error.is_some();
        let can_back = self.can_back && !retry;
        let can_forward = self.can_forward && !retry;

        let entity_select = cx.entity().clone();
        let entity_devtools = cx.entity().clone();
        let entity_clear = cx.entity().clone();
        let entity_copy = cx.entity().clone();
        let entity_dots = cx.entity().clone();
        let entity_fav = cx.entity().clone();
        let entity_zoom = cx.entity().clone();
        let entity_emu_preset = cx.entity().clone();
        let is_favorite = self.is_favorite();
        let zoom_pct = ((self.zoom.clamp(ZOOM_MIN, ZOOM_MAX) * 100.0).round() as u32).to_string();

        // Barre d'émulation device : preset + champs L × H + rotation + ✕.
        // Fine (~30 px), sous la toolbar, au-dessus de la page.
        let emu_bar = if self.emu_open {
            let preset_label = self
                .emulation
                .as_ref()
                .map(|d| d.label.clone())
                .unwrap_or_else(|| "Device".to_string());
            let active_slug = self.emulation.as_ref().map(|d| d.slug);
            let mut preset_button = div()
                .id("browser-emu-preset")
                .h(px(22.0))
                .px(px(8.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(theme.text)
                .hover(|s| s.bg(crate::theme::ink(0.06)))
                .on_click(move |_, _, cx| {
                    entity_emu_preset.update(cx, |this, cx| {
                        this.emu_preset_open = !this.emu_preset_open;
                        cx.notify();
                    });
                })
                .child(preset_label)
                .child(
                    crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                        .size(px(10.0))
                        .text_color(theme.text_muted),
                );
            if self.emu_preset_open {
                let mut list = div().flex().flex_col();
                for (ix, (slug, label)) in EMULATION_PRESETS.iter().enumerate() {
                    let slug_owned = slug.to_string();
                    let is_active = active_slug == Some(*slug);
                    list = list.child(
                        div()
                            .id(("browser-emu-preset-item", ix))
                            .h(px(26.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .text_color(if is_active {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .hover(|s| s.bg(crate::theme::ink(0.06)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.emu_pick_preset(&slug_owned, cx);
                                cx.notify();
                            }))
                            .child(*label),
                    );
                }
                preset_button = preset_button.relative().child(
                    popover::popover_card(&theme)
                        .w(px(200.0))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.emu_preset_open = false;
                            cx.notify();
                        }))
                        .child(list)
                        .into_any_element(),
                );
            }
            div()
                .id("browser-emu-bar")
                .h(px(30.0))
                .flex_none()
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .child(
                    crate::icons::icon(if self.emulation.as_ref().is_some_and(|d| d.mobile) {
                        icons::SMARTPHONE
                    } else {
                        icons::MONITOR
                    })
                    .size(px(13.0))
                    .text_color(theme.text_muted),
                )
                .child(preset_button)
                .child(
                    div()
                        .w(px(52.0))
                        .h(px(22.0))
                        .flex_none()
                        .child(self.emu_w.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child("×"),
                )
                .child(
                    div()
                        .w(px(52.0))
                        .h(px(22.0))
                        .flex_none()
                        .child(self.emu_h.clone()),
                )
                .child(
                    div()
                        .id("browser-emu-rotate")
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .when(self.emulation.is_none(), |el| el.opacity(0.4))
                        .hover(|s| s.bg(crate::theme::ink(0.06)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.emu_rotate(cx);
                        }))
                        .child(
                            crate::icons::icon(icons::ROTATE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .id("browser-emu-close")
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ink(0.06)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.emu_open = false;
                            this.emu_preset_open = false;
                            this.set_emulation(None, cx);
                        }))
                        .child(
                            crate::icons::icon(icons::CLOSE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .on_drag_move(cx.listener(Self::on_console_drag))
            .child(
                div()
                    .h(px(36.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(browser_button(
                        icons::ARROW_LEFT,
                        theme.text_muted,
                        can_back,
                        move || {
                            back.send(BrowserCommand::Back);
                        },
                        hint(
                            "Back",
                            Some("Return to the previous page in this tab's history."),
                            None,
                        ),
                    ))
                    .child(browser_button(
                        icons::ARROW_RIGHT,
                        theme.text_muted,
                        can_forward,
                        move || {
                            forward.send(BrowserCommand::Forward);
                        },
                        hint(
                            "Forward",
                            Some("Go forward to the next page in this tab's history."),
                            None,
                        ),
                    ))
                    .child(browser_button(
                        icons::REFRESH,
                        theme.text_muted,
                        true,
                        move || {
                            reload.send(if retry {
                                BrowserCommand::Retry
                            } else {
                                BrowserCommand::Reload
                            });
                        },
                        hint(
                            if retry { "Retry" } else { "Reload" },
                            Some(if retry {
                                "Try loading the page again after a failure."
                            } else {
                                "Reload the current page."
                            }),
                            None,
                        ),
                    ))
                    .child(
                        div()
                            .id("browser-address-row")
                            .group("browser-address")
                            .relative()
                            .h(px(28.0))
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(if address_focused {
                                theme.border_strong
                            } else {
                                theme.border
                            })
                            .when(address_focused, |el| el.shadow(theme.mark_ring()))
                            .bg(crate::theme::ink(0.04))
                            .px(px(6.0))
                            .child(div().flex_1().min_w_0().child(self.address.clone()))
                            // Suggestions de la barre d'adresse : jusqu'à 6
                            // lignes + « Search … », sous le champ, largeur
                            // du champ. ↑/↓/Entrée/Échap via ComposerInput.
                            .when(
                                address_focused && !self.suggestions.is_empty(),
                                |el| {
                                    el.child(self.render_suggestions(&theme, cx))
                                },
                            )
                            // Étoile des favoris : pleine si la page est
                            // favorite, clic = bascule (Ctrl+D équivalent).
                            .when(self.has_page(), |el| {
                                el.child(
                                    div()
                                        .id("browser-favorite")
                                        .size(px(20.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(crate::theme::ink(0.08)))
                                        .tooltip(hint(
                                            if is_favorite {
                                                "Remove from favorites"
                                            } else {
                                                "Add to favorites"
                                            },
                                            Some(
                                                "Save this page to your favorites, or remove it.",
                                            ),
                                            Some("Ctrl+D"),
                                        ))
                                        .on_click(move |_event, _window, cx| {
                                            entity_fav.update(cx, |this, cx| {
                                                this.toggle_favorite(cx);
                                            });
                                        })
                                        .child(
                                            crate::icons::icon(if is_favorite {
                                                icons::STAR_BOLD
                                            } else {
                                                icons::STAR
                                            })
                                            .size(px(13.0))
                                            .text_color(if is_favorite {
                                                theme.warning
                                            } else {
                                                theme.text_muted
                                            }),
                                        ),
                                )
                            })
                            // Puce de zoom : visible seulement si ≠ 100 %,
                            // un clic remet à 100 %.
                            .when(
                                (self.zoom.clamp(ZOOM_MIN, ZOOM_MAX) - ZOOM_DEFAULT).abs()
                                    > 0.0001,
                                |row| {
                                    row.child(
                                        div()
                                            .id("browser-zoom-chip")
                                            .flex_none()
                                            .px(px(6.0))
                                            .h(px(18.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(9.0))
                                            .bg(crate::theme::ink(0.08))
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted)
                                            .cursor_pointer()
                                            .hover(|s| s.bg(crate::theme::ink(0.14)))
                                            .tooltip(hint(
                                            "Reset zoom",
                                            Some("Return to 100% zoom."),
                                            Some("Ctrl+0"),
                                        ))
                                            .on_click(move |_, _, cx| {
                                                entity_zoom.update(cx, |this, cx| {
                                                    this.set_zoom(ZOOM_DEFAULT, cx);
                                                });
                                            })
                                            .child(zoom_pct.clone()),
                                    )
                                },
                            )
                            // Copy the shown address — Enter already submits.
                            // Visible au survol du champ ou quand il a le focus.
                            .child(
                                div()
                                    .id("browser-copy-url")
                                    .size(px(20.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .opacity(0.0)
                                    .group_hover("browser-address", |s| s.opacity(1.0))
                                    .when(address_focused, |el| el.opacity(1.0))
                                    .hover(|s| s.bg(crate::theme::ink(0.08)))
                                    .tooltip(hint(
                                            "Copy URL",
                                            Some("Copy the address shown in the bar to the clipboard."),
                                            None,
                                        ))
                                    .on_click(move |_event, _window, cx| {
                                        entity_copy.update(cx, |this, cx| {
                                            let url =
                                                this.address.read(cx).text().trim().to_owned();
                                            if !url.is_empty() {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    url.clone(),
                                                ));
                                                this.flash_status("URL copied", cx);
                                            }
                                        });
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::COPY)
                                            .size(px(12.0))
                                            .text_color(theme.text_muted),
                                    ),
                            ),
                    )
                    // « Add element » : mode sélection (croix, survol sondé,
                    // clic → pastille dans le composer, Échap pour quitter).
                    .child(browser_button_toggle(
                        icons::TARGET,
                        self.select_mode,
                        theme.text_muted,
                        move |_event, _window, cx| {
                            entity_select.update(cx, |this, cx| {
                                this.toggle_select_mode(cx);
                            });
                        },
                        hint(
                            if self.select_mode {
                                "Cancel selection"
                            } else {
                                "Add element to chat"
                            },
                            if self.select_mode {
                                None
                            } else {
                                Some(
                                    "Click any element on the page to attach it to your message as context.",
                                )
                            },
                            if self.select_mode { Some("Esc") } else { None },
                        ),
                    ))
                    // « DevTools » : tiroir console (Capture / filtres / resize).
                    .child(browser_button_toggle(
                        icons::TERMINAL,
                        self.show_console,
                        theme.text_muted,
                        move |_event, _window, cx| {
                            entity_devtools.update(cx, |this, cx| {
                                this.show_console = !this.show_console;
                                cx.notify();
                            });
                        },
                        hint(
                            if self.show_console {
                                "Close developer tools"
                            } else {
                                "Developer tools"
                            },
                            if self.show_console {
                                None
                            } else {
                                Some("Console, JavaScript and page screenshot.")
                            },
                            if self.show_console { None } else { Some("F12") },
                        ),
                    ))
                    // "…" overflow menu — same recipe as the header's `+` menu.
                    .child({
                        let mut dots = div()
                            .id("browser-dots")
                            .size(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ink(0.06)))
                            .tooltip(hint(
                                "More",
                                Some("New tab, history, zoom, find, device emulation and storage actions."),
                                None,
                            ))
                            .on_click(move |_event, _window, cx| {
                                entity_dots.update(cx, |this, cx| {
                                    this.dots_open = !this.dots_open;
                                    cx.notify();
                                });
                            })
                            .child(
                                crate::icons::icon(crate::icons::MORE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            );
                        if self.dots_open {
                            let items = self.menu_items();
                            let menu = popover::popover_card(&theme)
                                .w(px(264.0))
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.dots_open = false;
                                    cx.notify();
                                }))
                                .child({
                                    let mut col = div().flex().flex_col();
                                    for (ix, item) in items.iter().enumerate() {
                                        if item.separated {
                                            col = col.child(popover::menu_separator());
                                        }
                                        col =
                                            col.child(self.browser_menu_row(&theme, ix, item, cx));
                                    }
                                    col
                                })
                                .into_any_element();
                            dots = dots.relative().child(popover::anchored_menu_below_gap(
                                "browser-dots-menu",
                                menu,
                                None,
                                6.0,
                            ));
                        }
                        dots
                    }),
            )
            // Indication du mode « Add element » : « Click an element… · Esc »
            .when(self.select_mode, |el| {
                el.child(
                    div()
                        .h(px(24.0))
                        .flex_none()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(6.0))
                        .bg(theme.surface)
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(
                            crate::icons::icon(icons::TARGET)
                                .size(px(11.0))
                                .text_color(theme.accent),
                        )
                        .child("Click an element to add it to the chat · N for numbered picks · Esc to cancel"),
                )
            })
            // Visual loading bar when navigation is in flight
            .when(self.is_loading, |el| {
                el.child(
                    div()
                        .h(px(2.0))
                        .flex_none()
                        .w_full()
                        .relative()
                        .bg(theme.accent.opacity(0.2))
                        .child(
                            // indeterminate stripe occupying ~40% of the bar
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .h_full()
                                .w(gpui::relative(0.4))
                                .bg(theme.accent),
                        ),
                )
            })
            // Status toast / screenshot notification
            .when_some(self.status_toast.clone(), |el, status| {
                el.child(
                    div()
                        .flex_none()
                        .bg(theme.surface)
                        .border_b_1()
                        .border_color(theme.border)
                        .px(px(10.0))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(status),
                )
            })
            // Barre d'émulation device (preset + L × H + rotation + ✕).
            .child(emu_bar)
            // Zone principale : panneau History/Favorites quand ouvert,
            // sinon le viewport de page (image ou état vide).
            .child(div().flex_1().min_h_0().w_full().child(
                match self.render_panel(&theme, cx) {
                    Some(panel) => panel,
                    None => page.into_any_element(),
                },
            ))
            // Tiroir DevTools : console filtrable, redimensionnable, avec
            // Capture (screenshot) et « Add to chat » sur les lignes d'erreur.
            .when(self.show_console, |el| {
                let max_h = (f32::from(window.viewport_size().height) * 0.6).max(120.0);
                let drawer_h = self.console_height.clamp(120.0, max_h);
                let capture_commands = self.commands.clone();
                let filter = self.console_filter;
                el.child(
                    div()
                        .h(px(drawer_h))
                        .flex_none()
                        .relative()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        // Poignée de redimensionnement : 5 px flottant sur le
                        // bord haut du tiroir (pattern du panneau terminal).
                        .child(
                            div()
                                .id("console-drawer-resize")
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(px(5.0))
                                .cursor_ns_resize()
                                .hover(|s| s.bg(theme.border_strong))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                                        this.console_drag_anchor = Some((
                                            f32::from(event.position.y),
                                            this.console_height,
                                        ));
                                        cx.notify();
                                    }),
                                )
                                .on_drag(ConsoleDrawerResize, |_, _point: Point<Pixels>, _, cx| {
                                    cx.stop_propagation();
                                    cx.new(|_| ConsoleDragGhost)
                                }),
                        )
                        .child(
                            div()
                                .h(px(26.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(div().child("Console"))
                                // Filtres Tout / Erreurs / Avertissements.
                                .children(
                                    [
                                        (ConsoleFilter::All, "All"),
                                        (ConsoleFilter::Errors, "Errors"),
                                        (ConsoleFilter::Warnings, "Warnings"),
                                    ]
                                    .into_iter()
                                    .map(|(f, label)| {
                                        let active = filter == f;
                                        div()
                                            .id(("console-filter", f as usize))
                                            .h(px(18.0))
                                            .px(px(6.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(4.0))
                                            .cursor_pointer()
                                            .when(active, |s| {
                                                s.bg(crate::theme::ink(0.10))
                                                    .text_color(theme.text)
                                            })
                                            .when(!active, |s| {
                                                s.hover(|s| s.bg(crate::theme::ink(0.05)))
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.console_filter = f;
                                                cx.notify();
                                            }))
                                            .child(label)
                                    }),
                                )
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id("console-capture")
                                        .h(px(18.0))
                                        .px(px(6.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(crate::theme::ink(0.06)))
                                        .on_click(move |_, _, _| {
                                            capture_commands
                                                .send(BrowserCommand::CaptureScreenshot);
                                        })
                                        .child("Capture"),
                                )
                                .child(
                                    div()
                                        .id("clear-console-logs")
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(theme.text))
                                        .on_click(move |_event, _window, cx| {
                                            entity_clear.update(cx, |this, cx| {
                                                this.console_logs.clear();
                                                cx.notify();
                                            });
                                        })
                                        .child("Clear"),
                                )
                                .child(
                                    div()
                                        .id("close-console")
                                        .size(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(crate::theme::ink(0.08)))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_console = false;
                                            cx.notify();
                                        }))
                                        .child(
                                            crate::icons::icon(crate::icons::CLOSE)
                                                .size(px(10.0))
                                                .text_color(theme.text_muted),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .p(px(6.0))
                                .text_size(px(11.0))
                                .children(
                                    self.console_logs
                                        .iter()
                                        .filter(|log| console_log_visible(&log.level, filter))
                                        .enumerate()
                                        .map(|(ix, log)| {
                                            let color: gpui::Hsla = match log.level.as_str() {
                                                "error" => theme.danger,
                                                "warning" | "warn" => theme.warning,
                                                "input" => theme.text_muted,
                                                _ => theme.text,
                                            };
                                            let is_issue = matches!(
                                                log.level.as_str(),
                                                "error" | "warning" | "warn"
                                            );
                                            let row_group: SharedString =
                                                format!("console-row-{ix}").into();
                                            let mut row = div()
                                                .group(row_group.clone())
                                                .relative()
                                                .py(px(1.0))
                                                .text_color(color)
                                                .child(format!("[{}] {}", log.level, log.text));
                                            // « Add to chat » au survol des lignes
                                            // d'erreur : même mécanisme que les
                                            // éléments (pastille `<browser-element>`).
                                            if is_issue {
                                                let log_text = log.text.clone();
                                                let log_level = log.level.clone();
                                                row = row.child(
                                                    div()
                                                        .id(("console-add", ix))
                                                        .absolute()
                                                        .right_0()
                                                        .top_0()
                                                        .h_full()
                                                        .px(px(4.0))
                                                        .flex()
                                                        .items_center()
                                                        .rounded(px(3.0))
                                                        .cursor_pointer()
                                                        .opacity(0.0)
                                                        .group_hover(row_group, |s| {
                                                            s.opacity(1.0)
                                                        })
                                                        .hover(|s| s.bg(crate::theme::ink(0.08)))
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                cx.stop_propagation();
                                                                let context =
                                                                    ElementContext {
                                                                        url: this.url.clone(),
                                                                        title: this.title.clone(),
                                                                        tag: "console".into(),
                                                                        role: log_level.clone(),
                                                                        text: log_text.clone(),
                                                                        outer_html: format!(
                                                                            "[{}] {}",
                                                                            log_level, log_text
                                                                        ),
                                                                        ..ElementContext::default()
                                                                    };
                                                                this.flash_status(
                                                                    "Added console entry to chat",
                                                                    cx,
                                                                );
                                                                cx.emit(
                                                                    BrowserSurfaceEvent::AddElement(
                                                                        Box::new(context),
                                                                    ),
                                                                );
                                                            },
                                                        ))
                                                        .child(
                                                            div()
                                                                .text_size(px(10.0))
                                                                .text_color(theme.text_muted)
                                                                .child("Add to chat"),
                                                        ),
                                                );
                                            }
                                            row
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .h(px(26.0))
                                .flex_none()
                                .border_t_1()
                                .border_color(theme.border)
                                .px(px(6.0))
                                .child(self.console_input.clone()),
                        ),
                )
            })
            // Error banner
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex_none()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(error),
                )
            })
            // Dialogue modal « Clear Storage » (menu « … » / Settings).
            .when(self.clear_open, |el| {
                el.child(self.render_clear_dialog(&theme, cx))
            })
    }
}

struct BrowserViewport {
    child: AnyElement,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl IntoElement for BrowserViewport {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BrowserViewport {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.bounds.set(Some(bounds));
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// Localise une position souris dans la zone → (x, y) en pixels CSS de la
/// page. `view` Retransforme les coordonnées écran en pixels CSS :
/// soustraction de l'offset du cadre émulé, division par `zoom × fit`,
/// et rejet des points hors du cadre (la page ne les voit pas).
fn localize_in(
    page_bounds: &Rc<Cell<Option<Bounds<Pixels>>>>,
    position: &gpui::Point<Pixels>,
    view: ViewTransform,
) -> Option<(f32, f32)> {
    let bounds = page_bounds.get()?;
    let localized = bounds.localize(position)?;
    if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
        return None;
    }
    let fx = f32::from(localized.x) - view.ox;
    let fy = f32::from(localized.y) - view.oy;
    if fx < -0.5 || fy < -0.5 || fx > view.fw + 0.5 || fy > view.fh + 0.5 || view.div <= 0.0 {
        return None;
    }
    Some((fx / view.div, fy / view.div))
}

/// Infobulle d'un contrôle de la barre : titre, description et raccourci
/// optionnels, dans la même carte que le menu « … » (`popover_card` +
/// `frost::frosted` pour garder un seul calque de peinture).
struct HintTooltip {
    title: SharedString,
    desc: Option<SharedString>,
    shortcut: Option<SharedString>,
}

impl Render for HintTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::Theme::of(cx).clone();
        let card = popover::popover_card(&theme)
            .max_w(px(240.0))
            .px(px(10.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(self.title.clone()),
                    )
                    .when_some(self.shortcut.clone(), |el, shortcut| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(shortcut),
                        )
                    }),
            )
            .when_some(self.desc.clone(), |el, desc| {
                el.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(desc),
                )
            });
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
    }
}

/// Closure `.tooltip(...)` pour un contrôle de la barre : `desc` et
/// `shortcut` sont optionnels (les boutons simples n'ont qu'un titre).
fn hint(
    title: &'static str,
    desc: Option<&'static str>,
    shortcut: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| HintTooltip {
            title: title.into(),
            desc: desc.map(Into::into),
            shortcut: shortcut.map(Into::into),
        })
        .into()
    }
}

fn browser_button(
    icon_path: &'static str,
    color: gpui::Hsla,
    enabled: bool,
    action: impl Fn() + 'static,
    tooltip: impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static,
) -> impl IntoElement {
    let color = if enabled {
        color
    } else {
        gpui::hsla(0.0, 0.0, 0.6, 0.35)
    };
    let mut button = div()
        .id(icon_path)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .tooltip(tooltip);
    if enabled {
        button = button
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.06)))
            .on_click(move |_, _, _| action());
    }
    button.child(
        crate::icons::icon(icon_path)
            .size(px(14.0))
            .text_color(color),
    )
}

fn browser_button_toggle(
    icon_path: &'static str,
    active: bool,
    color: gpui::Hsla,
    action: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    tooltip: impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static,
) -> impl IntoElement {
    div()
        .id(icon_path)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .when(active, |el| {
            el.bg(crate::theme::glass_selected_bg())
                .shadow(crate::theme::glass_selected_shadows())
        })
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.08)))
        .tooltip(tooltip)
        .on_click(action)
        .child(
            crate::icons::icon(icon_path)
                .size(px(14.0))
                .text_color(color),
        )
}

/// Modèle de recherche par défaut (Google) — `KOMET_SEARCH_URL` et le
/// réglage Settings → Browser le remplacent.
pub const DEFAULT_SEARCH_URL: &str = "https://www.google.com/search?q={q}";

/// Ancien défaut (DuckDuckGo) : à la lecture des réglages, une valeur
/// sauvegardée exactement égale à celle-ci est migrée vers le nouveau défaut.
/// Toute autre valeur (preset choisi ou modèle custom) est conservée.
pub const LEGACY_DEFAULT_SEARCH_URL: &str = "https://duckduckgo.com/?q={q}";

/// `KOMET_SEARCH_URL` prime sur le réglage (compat scripts/dev), puis le
/// `BrowserPrefs.search_url` de la surface, sinon le défaut.
fn default_search_template() -> String {
    std::env::var("KOMET_SEARCH_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SEARCH_URL.to_string())
}

impl BrowserSurface {
    /// Modèle `{q}` effectif pour cette surface.
    fn search_template(&self) -> String {
        std::env::var("KOMET_SEARCH_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                let custom = self.prefs.search_url.trim();
                (!custom.is_empty()).then(|| custom.to_string())
            })
            .unwrap_or_else(|| DEFAULT_SEARCH_URL.to_string())
    }
}

pub fn normalize_url(value: &str) -> Result<String, String> {
    normalize_url_with(value, &default_search_template())
}

fn normalize_url_with(value: &str, search_template: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a URL or search term.".into());
    }
    if value.chars().any(char::is_control) {
        return Err("The address contains control characters.".into());
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("javascript:") {
        return Err("javascript: addresses are not supported.".into());
    }
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("about:")
    {
        return Ok(value.to_owned());
    }
    if value.contains("://") {
        return Err("Only HTTP and HTTPS addresses are supported.".into());
    }
    if is_local_target(value) {
        let expanded = value
            .strip_prefix(':')
            .map(|port| format!("localhost:{port}"))
            .unwrap_or_else(|| value.to_owned());
        return Ok(format!("http://{expanded}"));
    }
    if !value.chars().any(char::is_whitespace) && value.contains('.') {
        return Ok(format!("https://{value}"));
    }
    Ok(search_url(value, search_template))
}

/// Vrai pour les URL « page » : http(s) seulement (`about:`, `data:`… exclus).
fn is_web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Hostname of a URL for tab titles — scheme and port stripped, path/query
/// dropped. `None` for empty or unparseable input.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.trim();
    let rest = rest
        .strip_prefix("http://")
        .or_else(|| rest.strip_prefix("https://"))
        .unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // [::1]:8080 → ::1
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Hôte d'une URL d'historique pour les lignes du panneau (sans port).
fn history_host(url: &str) -> Option<String> {
    host_of(url)
}

/// Heure locale « HH:MM » pour la colonne de droite du panneau History.
fn format_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

fn percent_encode_query(query: &str) -> String {
    query
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>()
}

fn search_url(query: &str, template: &str) -> String {
    template.replace("{q}", &percent_encode_query(query))
}

/// Souche sans schéma : localhost, *.localhost, plages privées, ou « :PORT » seul.
fn is_local_target(value: &str) -> bool {
    if let Some(port) = value.strip_prefix(':') {
        // saisie nue « :3000 » → localhost
        return !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
    }
    let rest = value.split_once("://").map(|(_, r)| r).unwrap_or(value);
    let host_part = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let bare = if host_part.starts_with('[') {
        host_part
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host_part)
    } else if host_part.matches(':').count() > 1 {
        // littéral IPv6 sans port (ex. ::1) — un port exigerait des crochets
        host_part
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    let host = bare.to_ascii_lowercase();
    host == "localhost"
        || host == "0.0.0.0"
        || host == "::1"
        || host.ends_with(".localhost")
        || private_ipv4(&host)
}

fn private_ipv4(host: &str) -> bool {
    let Ok(ip) = host.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let [a, b, ..] = ip.octets();
    a == 0 || a == 10 || a == 127 || (a == 192 && b == 168) || (a == 172 && (16..=31).contains(&b))
}

/// GPUI key name → (key, code, windowsVirtualKeyCode) CDP.
/// Les caractères imprimables ne passent pas ici (Input.char le fait déjà).
fn gpui_key_to_cdp(key: &str) -> Option<(&'static str, &'static str, u32)> {
    match key {
        "backspace" => Some(("Backspace", "Backspace", 8)),
        "enter" => Some(("Enter", "Enter", 13)),
        "tab" => Some(("Tab", "Tab", 9)),
        "escape" => Some(("Escape", "Escape", 27)),
        "delete" => Some(("Delete", "Delete", 46)),
        "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "right" => Some(("ArrowRight", "ArrowRight", 39)),
        "up" => Some(("ArrowUp", "ArrowUp", 38)),
        "down" => Some(("ArrowDown", "ArrowDown", 40)),
        "home" => Some(("Home", "Home", 36)),
        "end" => Some(("End", "End", 35)),
        _ => None,
    }
}

// stubs RED (TDD #76) — implémentation dans la phase GREEN
fn push_console(logs: &mut VecDeque<ConsoleLog>, log: ConsoleLog) {
    if logs.len() >= 120 {
        logs.pop_front();
    }
    logs.push_back(log);
}

fn overlay_refresh_needed(prev_loading: bool, new_loading: bool, show_overlay: bool) -> bool {
    show_overlay && prev_loading && !new_loading
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use session::{UrlTracker, navigate_error_message};

    #[test]
    fn normalizes_addresses_and_searches() {
        assert_eq!(
            normalize_url("https://example.com/path").unwrap(),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_url("example.com/docs").unwrap(),
            "https://example.com/docs"
        );
        assert_eq!(
            normalize_url("localhost:5173").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_url("127.0.0.1:3000/app").unwrap(),
            "http://127.0.0.1:3000/app"
        );
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://www.google.com/search?q=komet%20browser"
        );
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
    }

    #[test]
    fn host_of_extracts_the_hostname() {
        assert_eq!(
            host_of("https://example.com/path?q=1").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("http://localhost:3000/app").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("example.com/docs").as_deref(), Some("example.com"));
        assert_eq!(host_of("https://[::1]:8080/").as_deref(), Some("::1"));
        assert_eq!(host_of("  "), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn effective_viewport_divides_css_size_and_scales_back() {
        // 100 % : inchangé.
        assert_eq!(
            effective_viewport((1280.0, 800.0), 1.0, 2.0),
            (1280, 800, 2.0)
        );
        // 200 % : viewport CSS divisé par 2, dsf doublé → même pixels image.
        let (w, h, dsf) = effective_viewport((1280.0, 800.0), 2.0, 2.0);
        assert_eq!((w, h), (640, 400));
        assert!((dsf - 4.0).abs() < 0.001);
        // 50 % : viewport CSS doublé.
        assert_eq!(
            effective_viewport((1000.0, 600.0), 0.5, 1.0),
            (2000, 1200, 0.5)
        );
        // Le zoom est borné à ZOOM_MAX (5×) → 100/5 = 20 px CSS.
        assert_eq!(effective_viewport((100.0, 100.0), 99.0, 1.0).0, 20);
        // Dimensions minimales 1 px même sur zone minuscule.
        assert_eq!(effective_viewport((10.0, 10.0), 5.0, 1.0).0, 2);
    }

    #[test]
    fn zoom_steps_through_levels_and_clamps() {
        // Depuis 100 % : + monte au palier suivant, - redescend.
        assert_eq!(zoom_step_level(1.0, 1), 1.1);
        assert_eq!(zoom_step_level(1.0, -1), 0.9);
        // Depuis un palier exact, + ne reste pas dessus.
        assert_eq!(zoom_step_level(1.25, 1), 1.5);
        // Bords : on ne dépasse ni MIN ni MAX.
        assert_eq!(zoom_step_level(ZOOM_MAX, 1), ZOOM_MAX);
        assert_eq!(zoom_step_level(ZOOM_MIN, -1), ZOOM_MIN);
        // Une valeur non palier (ex. zoom par défaut réglé à 1.3) rejoint le
        // palier supérieur/inférieur.
        assert_eq!(zoom_step_level(1.3, 1), 1.5);
        assert_eq!(zoom_step_level(1.3, -1), 1.25);
    }

    #[test]
    fn emulation_devices_have_expected_flags() {
        // Devices mobiles : viewport fixe + UA tactile.
        let iphone = emulation_device("iphone-14-pro", (800, 600)).unwrap();
        assert_eq!((iphone.width, iphone.height), (393, 852));
        assert!(iphone.mobile && iphone.touch);
        assert!(iphone.user_agent.is_some());
        // Responsive : dimensions libres, desktop.
        let resp = emulation_device("responsive", (500, 700)).unwrap();
        assert_eq!((resp.width, resp.height), (500, 700));
        assert!(!resp.mobile && !resp.touch && resp.user_agent.is_none());
        // Slug inconnu → None ; "off" n'est pas un device.
        assert!(emulation_device("off", (1, 1)).is_none());
        assert!(emulation_device("nope", (1, 1)).is_none());
        // Tous les slugs de EMULATION_PRESETS (sauf "off") produisent un device.
        for (slug, _) in EMULATION_PRESETS.iter().filter(|(s, _)| *s != "off") {
            assert!(
                emulation_device(slug, (800, 600)).is_some(),
                "missing device for {slug}"
            );
        }
    }

    #[test]
    fn rejects_empty_and_script_addresses() {
        assert!(normalize_url("  ").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("example.com\n").is_ok());
    }

    #[test]
    fn normalizes_port_only_and_local_tld_and_private_ranges() {
        assert_eq!(normalize_url(":3000").unwrap(), "http://localhost:3000");
        assert_eq!(
            normalize_url("app.localhost:5173").unwrap(),
            "http://app.localhost:5173"
        );
        assert_eq!(
            normalize_url("app.localhost").unwrap(),
            "http://app.localhost"
        );
        assert_eq!(
            normalize_url("172.20.1.5:8080").unwrap(),
            "http://172.20.1.5:8080"
        );
        assert_eq!(normalize_url("172.16.0.1").unwrap(), "http://172.16.0.1");
    }

    #[test]
    fn real_host_parsing_beats_prefix_matching() {
        // localhost.evil.com est un domaine public : jamais de http local.
        assert_eq!(
            normalize_url("localhost.evil.com").unwrap(),
            "https://localhost.evil.com"
        );
        assert_eq!(
            normalize_url("localhost.evil.com:8080").unwrap(),
            "https://localhost.evil.com:8080"
        );
        // 10.999.1 n'est pas une IP des plages privées → domaine public
        assert_eq!(
            normalize_url("192.168.evil.com").unwrap(),
            "https://192.168.evil.com"
        );
    }

    #[test]
    fn search_engine_is_configurable_via_env() {
        // unsafe requis par edition 2024 ; process-global, ok car tests monothread ici.
        unsafe {
            std::env::set_var("KOMET_SEARCH_URL", "https://searx.example/?q={q}");
        }
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://searx.example/?q=komet%20browser"
        );
        unsafe {
            std::env::remove_var("KOMET_SEARCH_URL");
        }
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://www.google.com/search?q=komet%20browser"
        );
    }

    #[test]
    fn url_tracker_follows_main_frame_only() {
        let mut tracker = UrlTracker::default();

        // frameNavigated sans parentId = frame principale : fixe l'id + l'URL
        assert_eq!(
            tracker.on_nav_event(
                "Page.frameNavigated",
                &json!({"frame": {"id": "F1", "url": "http://a/"}})
            ),
            Some("http://a/".to_owned())
        );
        assert_eq!(tracker.main_frame_id(), Some("F1"));

        // tilt SPA sur la frame principale : URL dans params.url (PAS params.frame.url)
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F1", "url": "http://a/route2"})
            ),
            Some("http://a/route2".to_owned())
        );

        // iframe : navigatedWithinDocument ignoré
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F9", "url": "http://if/x"})
            ),
            None
        );
        // iframe : frameNavigated (avec parentId) ignoré
        assert_eq!(
            tracker.on_nav_event(
                "Page.frameNavigated",
                &json!({"frame": {"id": "F9", "parentId": "F1", "url": "about:blank"}})
            ),
            None
        );
        // la frame principale reste connue après une iframe
        assert_eq!(tracker.main_frame_id(), Some("F1"));
    }

    #[test]
    fn url_tracker_skips_navigated_within_document_when_main_frame_unknown() {
        let mut tracker = UrlTracker::default();
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F1", "url": "http://a/r"})
            ),
            None
        );
    }

    #[test]
    fn url_tracker_reports_loading_for_main_frame() {
        let mut tracker = UrlTracker::default();
        tracker.on_nav_event(
            "Page.frameNavigated",
            &json!({"frame": {"id": "F1", "url": "http://a/"}}),
        );

        // frame principale : Started → true, Stopped → false
        assert_eq!(
            tracker.on_loading_event("Page.frameStartedLoading", "F1"),
            Some(true)
        );
        assert_eq!(
            tracker.on_loading_event("Page.frameStoppedLoading", "F1"),
            Some(false)
        );
        // autre frame : non pertinent
        assert_eq!(
            tracker.on_loading_event("Page.frameStartedLoading", "F9"),
            None
        );
        assert_eq!(
            tracker.on_loading_event("Page.frameStoppedLoading", "F9"),
            None
        );
        // événement inconnu
        assert_eq!(tracker.on_loading_event("Page.loadEventFired", "F1"), None);
    }

    #[test]
    fn console_filter_matches_levels() {
        assert!(console_log_visible("error", ConsoleFilter::All));
        assert!(console_log_visible("log", ConsoleFilter::All));
        assert!(console_log_visible("error", ConsoleFilter::Errors));
        assert!(!console_log_visible("warning", ConsoleFilter::Errors));
        assert!(console_log_visible("warning", ConsoleFilter::Warnings));
        assert!(console_log_visible("warn", ConsoleFilter::Warnings));
        assert!(!console_log_visible("error", ConsoleFilter::Warnings));
        assert!(!console_log_visible("log", ConsoleFilter::Errors));
    }

    #[test]
    fn maps_gpui_keys_to_cdp_special_keys() {
        assert_eq!(
            gpui_key_to_cdp("backspace"),
            Some(("Backspace", "Backspace", 8))
        );
        assert_eq!(gpui_key_to_cdp("enter"), Some(("Enter", "Enter", 13)));
        assert_eq!(gpui_key_to_cdp("tab"), Some(("Tab", "Tab", 9)));
        assert_eq!(gpui_key_to_cdp("escape"), Some(("Escape", "Escape", 27)));
        assert_eq!(gpui_key_to_cdp("delete"), Some(("Delete", "Delete", 46)));
        assert_eq!(
            gpui_key_to_cdp("left"),
            Some(("ArrowLeft", "ArrowLeft", 37))
        );
        assert_eq!(
            gpui_key_to_cdp("right"),
            Some(("ArrowRight", "ArrowRight", 39))
        );
        assert_eq!(gpui_key_to_cdp("up"), Some(("ArrowUp", "ArrowUp", 38)));
        assert_eq!(
            gpui_key_to_cdp("down"),
            Some(("ArrowDown", "ArrowDown", 40))
        );
        assert_eq!(gpui_key_to_cdp("home"), Some(("Home", "Home", 36)));
        assert_eq!(gpui_key_to_cdp("end"), Some(("End", "End", 35)));
        // les caractères imprimables passent déjà via Input.char → pas de mapping
        assert_eq!(gpui_key_to_cdp("a"), None);
        assert_eq!(gpui_key_to_cdp("inconnu"), None);
    }

    #[test]
    fn maps_modifier_flags_to_cdp_bits() {
        assert_eq!(cdp_modifiers(false, false, false, false), 0);
        // signature : alt, ctrl, shift, meta
        assert_eq!(cdp_modifiers(false, true, false, false), 2);
        assert_eq!(cdp_modifiers(true, true, false, false), 1 | 2);
        assert_eq!(cdp_modifiers(false, false, true, false), 8);
        assert_eq!(cdp_modifiers(false, false, false, true), 4);
    }

    #[test]
    fn error_text_becomes_friendly_message() {
        let m = navigate_error_message("net::ERR_CONNECTION_REFUSED", "http://localhost:5173/");
        assert!(m.contains("localhost:5173"), "doit citer l'adresse : {m}");
        assert!(
            m.to_lowercase().contains("serveur"),
            "doit suggérer le serveur : {m}"
        );
        assert!(
            m.contains("Réessayer"),
            "doit pointer le bouton Retry : {m}"
        );

        let other = navigate_error_message("net::ERR_NAME_NOT_RESOLVED", "https://nope.example");
        assert!(other.contains("nope.example"), "{other}");
        assert!(
            other.contains("net::ERR_NAME_NOT_RESOLVED"),
            "garde le détail brut : {other}"
        );
    }

    #[test]
    fn console_ring_caps_at_120_keeping_latest() {
        let mut logs = VecDeque::new();
        for i in 0..130 {
            push_console(
                &mut logs,
                ConsoleLog {
                    level: "log".into(),
                    text: format!("m{i}"),
                },
            );
        }
        assert_eq!(logs.len(), 120, "le ring garde au plus 120 entrées");
        assert_eq!(
            logs.front().unwrap().text,
            "m10",
            "les 10 plus anciennes sont évacuées"
        );
        assert_eq!(logs.back().unwrap().text, "m129", "la plus récente reste");
    }

    #[test]
    fn overlay_refreshes_only_when_loading_finishes_while_shown() {
        // seul cas de rafraîchissement : la page passe de « en charge » à
        // « finie » pendant que l'overlay inspecteur est affiché
        assert!(overlay_refresh_needed(true, false, true));
        assert!(!overlay_refresh_needed(true, false, false));
        assert!(!overlay_refresh_needed(false, false, true));
        assert!(!overlay_refresh_needed(true, true, true));
        assert!(!overlay_refresh_needed(false, true, true));
    }
}
