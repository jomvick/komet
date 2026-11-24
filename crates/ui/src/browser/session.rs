//! Contrôleur de session Chromium/CDP : thread dédié, canal de commandes
//! UI → CDP (`BrowserCommand`) et flux d'événements CDP → UI
//! (`BrowserEvent`).
//!
//! Extrait de `browser.rs` (phase 1 du plan navigateur) : la couche session
//! ne connaît rien de gpui côté rendu — elle reçoit des commandes, parle à
//! Chrome par WebSocket et pousse des événements (`push_event`/`push_event_lossy`).
//! Les structures UI (`BrowserSurface`, rendu, entrées) restent dans
//! `browser.rs` ; le lancement/pool Chromium est dans `process.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect as ws_connect};

use super::cdp;
use super::cdp::{ConsoleLog, FindPageResult, OverlayElement};
use super::clear::ClearSelection;
use super::downloads;
use super::element::{ElementContext, ElementHover};
use super::process::acquire_browser;
use super::{DEFAULT_VIEWPORT, EmulatedDevice};

type WsStream = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

/// Canal de commandes UI → CDP, doublé du self-pipe de réveil.
#[derive(Clone)]
pub(crate) struct CommandPipe {
    pub(crate) tx: mpsc::Sender<BrowserCommand>,
    pub(crate) wake_fd: Option<i32>,
}

impl CommandPipe {
    pub(crate) fn send(&self, command: BrowserCommand) {
        if self.tx.send(command).is_err() {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(fd) = self.wake_fd {
            // non-bloquant : pipe plein = octets en attente = déjà réveillé
            unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
        }
    }
}

// wake CDP → UI : le thread CDP signale chaque lot d'événements ; la tâche
// gpui attend dessus (futures channel) au lieu de timer toutes les 8 ms.
pub(crate) type WakeTx = futures::channel::mpsc::UnboundedSender<()>;
pub(crate) type WakeRx = futures::channel::mpsc::UnboundedReceiver<()>;

fn push_event(events: &mpsc::SyncSender<BrowserEvent>, wake: &WakeTx, event: BrowserEvent) {
    let _ = events.send(event);
    let _ = wake.unbounded_send(());
}

/// Pour les frames : channel plein → frame perdue (l'UI garde la dernière).
fn push_event_lossy(events: &mpsc::SyncSender<BrowserEvent>, wake: &WakeTx, event: BrowserEvent) {
    let _ = events.try_send(event);
    let _ = wake.unbounded_send(());
}

pub enum BrowserCommand {
    Back,
    Forward,
    GetNavHistory,
    Reload,
    Retry,
    Navigate(String),
    KeyPress {
        key: &'static str,
        code: &'static str,
        vk: Option<u32>,
        modifiers: u32,
    },
    KeyRelease {
        key: &'static str,
        code: &'static str,
        modifiers: u32,
    },
    InsertText(String),
    MousePress {
        x: f32,
        y: f32,
        button: &'static str,
        count: u32,
    },
    MouseRelease {
        x: f32,
        y: f32,
        button: &'static str,
        count: u32,
    },
    MouseMove {
        x: f32,
        y: f32,
    },
    Scroll {
        x: f32,
        y: f32,
        delta_x: f64,
        delta_y: f64,
    },
    /// Sonde de survol du mode « Add element » : `elementFromPoint` +
    /// rectangle/pastille, ~25×/s max côté UI.
    ProbeAt {
        x: f32,
        y: f32,
    },
    /// Clic en mode sélection : décrit l'élément sous le point et émet
    /// `BrowserSurfaceEvent::AddElement`.
    DescribeAt {
        x: f32,
        y: f32,
    },
    /// Find in Page : `step` 0 = nouvelle recherche, ±1 = occurrence
    /// suivante/précédente (même requête, sans rescanner le DOM).
    FindInPage {
        query: String,
        step: i32,
    },
    /// Ferme Find in Page : retire surlignages et style injecté.
    ClearFind,
    /// Device emulation : `Some` applique viewport mobile/UA/touch,
    /// `None` revient au viewport fluide de la zone.
    SetEmulation {
        device: Option<EmulatedDevice>,
    },
    /// Dialogue « Clear Storage » confirmé : `origins` = origines connues
    /// (historique + favoris + page courante) pour `Storage.clearDataForOrigin`.
    ClearBrowsingData {
        selection: ClearSelection,
        origins: Vec<String>,
    },
    TypeText(String),
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    CaptureScreenshot,
    ToggleOverlay,
    Evaluate(String),
    /// Réglage du dossier de téléchargements (Settings → Browser) : le
    /// ré-applique sur la session ouverte.
    SetDownloadDir(String),
    /// Le panneau browser s'affiche / se cache : caché, la session CDP se
    /// gare (stopScreencast, attente bloquante — zéro réveil CPU).
    SetVisible(bool),
    /// L'UI a consommé la frame ; le thread CDP acquitte Chrome (backpressure :
    /// au plus une frame non affichée, jamais de rattrapage coûteux).
    AckFrame(u64),
    Shutdown,
}

pub enum BrowserEvent {
    Page {
        image: Arc<RenderImage>,
        title: String,
        url: String,
        session_id: u64,
    },
    Title(String),
    Loading(bool),
    NavHistory {
        can_back: bool,
        can_forward: bool,
    },
    Elements(Vec<OverlayElement>),
    Console(ConsoleLog),
    /// Résultat d'un `ProbeAt` : `None` quand plus rien ne survole
    /// (`elementFromPoint` → null).
    ElementHover(Option<ElementHover>),
    /// Résultat d'un `DescribeAt` : contexte complet de l'élément cliqué.
    ElementContext(Box<ElementContext>),
    /// Résultat d'un `FindInPage` : nombre d'occurrences et index courant
    /// (-1 quand aucune correspondance).
    FindResult {
        count: usize,
        index: i64,
    },
    Screenshot(Vec<u8>),
    /// `Browser.downloadWillBegin` : un téléchargement démarre.
    DownloadStart {
        id: String,
        filename: String,
        url: String,
    },
    /// Progression (`receivedBytes`/`totalBytes`) ; `total` = `None` si la
    /// taille est inconnue (`-1` CDP).
    DownloadProgress {
        id: String,
        received: u64,
        total: Option<u64>,
    },
    /// État terminal : `ok` = fichier écrit, sinon annulé ; `path` est le
    /// chemin final quand il est connu.
    DownloadDone {
        id: String,
        ok: bool,
        path: Option<String>,
    },
    /// Une requête réseau démarre (`Network.requestWillBeSent`).
    NetworkStart {
        id: String,
        url: String,
        method: String,
    },
    /// Réponse reçue (`Network.responseReceived`).
    NetworkResponded {
        id: String,
        status: u16,
    },
    /// Échec réseau (`Network.loadingFailed`) ; `canceled` = abort côté page.
    NetworkFailed {
        id: String,
        error: String,
        canceled: bool,
    },
    Failed(String),
    /// Dégradation signalée à l'utilisateur (ex. repli --no-sandbox).
    Warning(String),
}

enum SessionResult {
    Retry,
    Shutdown,
}

fn gpui_render_image(image: RgbaImage) -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![Frame::new(image)]))
}

/// Suit quelle frame CDP est la frame principale, pour ne jamais confondre URL
/// d'iframe et URL de la barre d'adresse.
#[derive(Default)]
pub(crate) struct UrlTracker {
    main_frame_id: Option<String>,
}

impl UrlTracker {
    #[cfg(test)]
    pub(crate) fn main_frame_id(&self) -> Option<&str> {
        self.main_frame_id.as_deref()
    }

    /// Renvoie la nouvelle URL courante si l'événement affecte la frame principale.
    /// `Page.navigatedWithinDocument` porte l'URL dans `params.url` (pas
    /// `params.frame.url`) — c'était le bug de la barre d'URL sur les SPA.
    pub(crate) fn on_nav_event(&mut self, method: &str, params: &Value) -> Option<String> {
        if method == "Page.navigatedWithinDocument" {
            let frame_id = params.get("frameId").and_then(Value::as_str)?;
            if self.main_frame_id.as_deref() != Some(frame_id) {
                return None;
            }
            return params.get("url").and_then(Value::as_str).map(str::to_owned);
        }
        if method == "Page.frameNavigated" {
            let frame = params.get("frame")?;
            if frame.get("parentId").is_some() {
                // event frameNavigated est aussi émis pour les iframes
                return None;
            }
            let url = frame.get("url").and_then(Value::as_str)?.to_owned();
            if let Some(id) = frame.get("id").and_then(Value::as_str) {
                self.main_frame_id = Some(id.to_owned());
            }
            return Some(url);
        }
        None
    }

    pub(crate) fn on_loading_event(&self, method: &str, frame_id: &str) -> Option<bool> {
        if self.main_frame_id.as_deref() != Some(frame_id) {
            return None;
        }
        match method {
            "Page.frameStartedLoading" => Some(true),
            "Page.frameStoppedLoading" => Some(false),
            _ => None,
        }
    }
}

pub(crate) fn navigate_error_message(error_text: &str, url: &str) -> String {
    if error_text.contains("ERR_CONNECTION_REFUSED") {
        format!("{url} ne répond pas — lance ton serveur, puis clique Recharger/Réessayer.")
    } else if error_text.contains("ERR_NAME_NOT_RESOLVED") {
        format!("{url} : adresse introuvable ({error_text}).")
    } else {
        format!("{url} : {error_text}")
    }
}

pub(crate) fn run_client(
    commands: Receiver<BrowserCommand>,
    events: mpsc::SyncSender<BrowserEvent>,
    wake: &WakeTx,
    wake_read: Option<i32>,
    initial_url: String,
    profile_key: String,
    download_dir: PathBuf,
) {
    let mut current_url = initial_url;
    // Le dossier de téléchargements voulu survit aux tentatives : appliqué
    // au démarrage de chaque session et mis à jour par SetDownloadDir où
    // qu'il arrive (session active, surface garée, attente d'échec).
    let mut download_dir = download_dir;
    loop {
        // Chrome partagé du pool (keeper thread) ; la lease rendue au bout de
        // la session décrémente le refcount — à 0 le pool tue le process.
        let result = match acquire_browser(&profile_key) {
            Ok((page_ws, warning, _lease)) => {
                if let Some(warning) = warning {
                    push_event(&events, wake, BrowserEvent::Warning(warning));
                }
                run_session(
                    &commands,
                    &events,
                    wake,
                    wake_read,
                    &mut current_url,
                    &page_ws,
                    &mut download_dir,
                )
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(SessionResult::Shutdown) => break,
            Ok(SessionResult::Retry) => {}
            Err(error) => {
                push_event(&events, wake, BrowserEvent::Failed(error));
                loop {
                    match commands.recv_timeout(Duration::from_secs(30)) {
                        // une URL tapée pendant l'échec relance le moteur
                        // comme Réessayer (sinon la surface reste morte)
                        Ok(BrowserCommand::Navigate(url)) => {
                            current_url = url;
                            break;
                        }
                        Ok(BrowserCommand::Retry) => break,
                        // changement de dossier pendant l'échec : retenu
                        // pour la session suivante
                        Ok(BrowserCommand::SetDownloadDir(path)) => {
                            download_dir = PathBuf::from(path);
                        }
                        Ok(BrowserCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
    }
}

fn run_session(
    commands: &Receiver<BrowserCommand>,
    events: &mpsc::SyncSender<BrowserEvent>,
    wake: &WakeTx,
    wake_read: Option<i32>,
    current_url: &mut String,
    page_ws_url: &str,
    download_dir: &mut PathBuf,
) -> Result<SessionResult, String> {
    let (mut socket, _response) = ws_connect(page_ws_url).map_err(|e| e.to_string())?;
    // id de la cible CDP (pour Target.closeTarget à la fermeture de la surface)
    let target_id = page_ws_url.rsplit('/').next().unwrap_or("").to_owned();

    let mut next_id: u64 = 1;

    // Initialize CDP session
    socket
        .send(Message::Text(cdp::page_enable(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    socket
        .send(Message::Text(cdp::runtime_enable(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // Réseau + téléchargements : avant la première navigation, pour ne rien
    // rater. setDownloadBehavior s'applique au navigateur entier ; le dossier
    // existe (resolve_download_dir), sinon Chrome refuse le comportement.
    socket
        .send(Message::Text(cdp::network_enable(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;
    let download_path = download_dir.to_string_lossy().into_owned();
    socket
        .send(Message::Text(cdp::set_download_behavior(
            next_id,
            &download_path,
        )))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // target=_blank / window.open : attacher automatiquement les popups pour
    // pouvoir les rediriger vers la vue courante (voir Target.attachedToTarget)
    socket
        .send(Message::Text(cdp::set_auto_attach(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // Set initial device metrics to match default viewport
    socket
        .send(Message::Text(cdp::set_device_metrics(
            next_id,
            DEFAULT_VIEWPORT.0,
            DEFAULT_VIEWPORT.1,
            1.0,
            false,
        )))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    socket
        .send(Message::Text(cdp::start_screencast(
            next_id,
            DEFAULT_VIEWPORT.0,
            DEFAULT_VIEWPORT.1,
            "jpeg",
            90,
        )))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // La navigation part APRÈS le startScreencast : le screencast capture
    // ainsi la page dès son premier paint. (Naviguer avant = la page finit
    // de peindre avant l'attachement et Chrome n'émet plus aucun frame.)
    // Sans URL (surface ouverte à vide) on reste sur about:blank.
    let mut navigate_request_ids = std::collections::HashSet::new();
    if !current_url.trim().is_empty() {
        socket
            .send(Message::Text(cdp::page_navigate(next_id, current_url)))
            .map_err(|e| e.to_string())?;
        navigate_request_ids.insert(next_id);
        next_id += 1;
    }

    let mut page_title = String::new();
    let mut screenshot_request_id: Option<u64> = None;
    let mut title_request_id: Option<u64> = None;
    let mut overlay_request_id: Option<u64> = None;
    let mut dialog_request_id: Option<u64> = None;
    let mut nav_history_request_id: Option<u64> = None;
    let mut nav_history_step: i32 = 0;
    let mut eval_request_ids = std::collections::HashSet::new();
    let mut probe_ids = std::collections::HashSet::new();
    let mut describe_id: Option<u64> = None;
    let mut find_id: Option<u64> = None;
    let mut url_tracker = UrlTracker::default();
    // Progression de téléchargement : au plus un événement par guid toutes
    // les 250 ms (les états terminaux passent toujours).
    let mut download_progress_last: HashMap<String, Instant> = HashMap::new();

    // Set non-blocking on the underlying TCP stream so we can multiplex commands and frames
    set_socket_nonblocking(&mut socket, true)?;

    // frame screencast envoyée à l'UI mais pas encore acquittée par elle
    let mut pending_frame: Option<u64> = None;
    let mut visible = true;
    let mut last_dims = (DEFAULT_VIEWPORT.0, DEFAULT_VIEWPORT.1, 1.0f32);
    // `mobile` du dernier Emulation.setDeviceMetricsOverride — piloté par
    // BrowserCommand::SetEmulation (émulation device).
    let mut emulation_mobile = false;

    loop {
        // parké : attente bloquante, zéro réveil tant que le panneau est caché
        if !visible {
            match commands.recv() {
                Ok(BrowserCommand::SetVisible(true)) => {
                    visible = true;
                    let (w, h, scale) = last_dims;
                    send_screencast_restart(
                        &mut socket,
                        &mut next_id,
                        w,
                        h,
                        scale,
                        emulation_mobile,
                    )?;
                }
                Ok(BrowserCommand::SetDownloadDir(path)) => {
                    // Retenu pendant le parking et appliqué tout de suite : le
                    // socket vit, le prochain téléchargement utilisera ce
                    // dossier sans attendre le réveil.
                    *download_dir = PathBuf::from(&path);
                    let _ = socket.send(Message::Text(cdp::set_download_behavior(next_id, &path)));
                    next_id += 1;
                }
                Ok(BrowserCommand::Shutdown) => {
                    let _ = socket.send(Message::Text(cdp::close_target(next_id, &target_id)));
                    return Ok(SessionResult::Shutdown);
                }
                Ok(BrowserCommand::Retry) => return Ok(SessionResult::Retry),
                Ok(_) => {}
                // plus d'UI : la surface est partie
                Err(_) => return Ok(SessionResult::Shutdown),
            }
            continue;
        }

        // 1. Process pending UI commands
        loop {
            let cmd = match commands.try_recv() {
                Ok(cmd) => cmd,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(SessionResult::Shutdown),
            };
            match cmd {
                BrowserCommand::Back | BrowserCommand::Forward | BrowserCommand::GetNavHistory => {
                    let step = match cmd {
                        BrowserCommand::Back => -1,
                        BrowserCommand::Forward => 1,
                        _ => 0,
                    };
                    nav_history_request_id = Some(next_id);
                    nav_history_step = step;
                    socket
                        .send(Message::Text(cdp::get_navigation_history(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Reload => {
                    push_event_lossy(events, wake, BrowserEvent::Loading(true));
                    socket
                        .send(Message::Text(cdp::page_reload(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Retry => return Ok(SessionResult::Retry),
                BrowserCommand::Navigate(url) => {
                    push_event_lossy(events, wake, BrowserEvent::Loading(true));
                    socket
                        .send(Message::Text(cdp::page_navigate(next_id, &url)))
                        .map_err(|e| e.to_string())?;
                    navigate_request_ids.insert(next_id);
                    next_id += 1;
                    *current_url = url;
                }
                BrowserCommand::KeyPress {
                    key,
                    code,
                    vk,
                    modifiers,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_key_event(
                            next_id,
                            "rawKeyDown",
                            key,
                            code,
                            vk,
                            modifiers,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::KeyRelease {
                    key,
                    code,
                    modifiers,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_key_event(
                            next_id, "keyUp", key, code, None, modifiers,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::InsertText(text) => {
                    socket
                        .send(Message::Text(cdp::insert_text(next_id, &text)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MousePress {
                    x,
                    y,
                    button,
                    count,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_click(
                            next_id,
                            "mousePressed",
                            x,
                            y,
                            button,
                            count,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MouseRelease {
                    x,
                    y,
                    button,
                    count,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_click(
                            next_id,
                            "mouseReleased",
                            x,
                            y,
                            button,
                            count,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MouseMove { x, y } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_move(next_id, x, y)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Scroll {
                    x,
                    y,
                    delta_x,
                    delta_y,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_wheel(
                            next_id, x, y, delta_x, delta_y,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::TypeText(text) => {
                    for ch in text.chars() {
                        socket
                            .send(Message::Text(cdp::dispatch_char_event(
                                next_id,
                                &ch.to_string(),
                            )))
                            .map_err(|e| e.to_string())?;
                        next_id += 1;
                    }
                }
                BrowserCommand::Resize {
                    width,
                    height,
                    scale,
                } => {
                    let dims_changed = last_dims.0 != width || last_dims.1 != height;
                    last_dims = (width, height, scale);
                    if dims_changed {
                        // redémarre le screencast avec les nouvelles dimensions ;
                        // à dimensions identiques, rien à faire (pas de restart)
                        send_screencast_restart(
                            &mut socket,
                            &mut next_id,
                            width,
                            height,
                            scale,
                            emulation_mobile,
                        )?;
                    } else {
                        socket
                            .send(Message::Text(cdp::set_device_metrics(
                                next_id,
                                width,
                                height,
                                scale,
                                emulation_mobile,
                            )))
                            .map_err(|e| e.to_string())?;
                        next_id += 1;
                    }
                }
                BrowserCommand::SetEmulation { device } => {
                    emulation_mobile = device.as_ref().is_some_and(|d| d.mobile);
                    // User-Agent : chaîne vide = retour à l'UA du Chrome.
                    socket
                        .send(Message::Text(cdp::set_user_agent_override(
                            next_id,
                            device.as_ref().and_then(|d| d.user_agent),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                    let touch = device.as_ref().is_some_and(|d| d.touch);
                    socket
                        .send(Message::Text(cdp::set_touch_emulation(next_id, touch)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                    // Ré-applique les métriques courantes avec le flag
                    // `mobile` — le Resize suivant (viewport device) suit.
                    let (w, h, scale) = last_dims;
                    socket
                        .send(Message::Text(cdp::set_device_metrics(
                            next_id,
                            w,
                            h,
                            scale,
                            emulation_mobile,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::SetVisible(false) => {
                    visible = false;
                    let _ = socket.send(Message::Text(cdp::stop_screencast(next_id)));
                    next_id += 1;
                }
                BrowserCommand::SetVisible(true) => {}
                BrowserCommand::AckFrame(session_id) => {
                    if pending_frame == Some(session_id) {
                        pending_frame = None;
                        let _ = socket.send(Message::Text(cdp::screencast_frame_ack(
                            next_id, session_id,
                        )));
                        next_id += 1;
                    }
                }
                BrowserCommand::Shutdown => {
                    // la surface ferme SA cible ; le Chrome du pool survit si
                    // d'autres surfaces l'utilisent (kill à 0 ref par le pool)
                    let _ = socket.send(Message::Text(cdp::close_target(next_id, &target_id)));
                    return Ok(SessionResult::Shutdown);
                }
                BrowserCommand::CaptureScreenshot => {
                    screenshot_request_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::capture_screenshot(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::ToggleOverlay => {
                    overlay_request_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            &cdp::extract_elements_script(),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::ProbeAt { x, y } => {
                    probe_ids.insert(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            &cdp::probe_at_script(x, y),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::DescribeAt { x, y } => {
                    describe_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            &cdp::describe_at_script(x, y),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::FindInPage { query, step } => {
                    // Une seule recherche « en vol » compte : les réponses
                    // arrivent dans l'ordre d'envoi, seule la dernière est
                    // à jour — les anciennes sont ignorées par l'id.
                    find_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            &cdp::find_in_page_script(&query, step),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::ClearFind => {
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            &cdp::find_clear_script(),
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::ClearBrowsingData { selection, origins } => {
                    // Cookies/cache : une commande chacun, effet profil.
                    if selection.cookies {
                        socket
                            .send(Message::Text(cdp::clear_browser_cookies(next_id)))
                            .map_err(|e| e.to_string())?;
                        next_id += 1;
                    }
                    if selection.cache {
                        socket
                            .send(Message::Text(cdp::clear_browser_cache(next_id)))
                            .map_err(|e| e.to_string())?;
                        next_id += 1;
                    }
                    // Données de sites : par origine connue (historique +
                    // favoris + page courante), pas de « toutes origines ».
                    if selection.site_data {
                        for origin in origins {
                            socket
                                .send(Message::Text(cdp::clear_data_for_origin(next_id, &origin)))
                                .map_err(|e| e.to_string())?;
                            next_id += 1;
                        }
                    }
                }
                BrowserCommand::Evaluate(expr) => {
                    eval_request_ids.insert(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(next_id, &expr)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::SetDownloadDir(path) => {
                    // retenu pour les sessions à venir (Retry) et appliqué
                    // tout de suite sur celle-ci
                    *download_dir = PathBuf::from(&path);
                    let _ = socket.send(Message::Text(cdp::set_download_behavior(next_id, &path)));
                    next_id += 1;
                }
            }
        }

        // 2. Attendre du travail : socket CDP ou self-pipe de commandes
        // (le poll bloque — plus de boucle occupée à 4 ms)
        #[cfg(target_os = "linux")]
        match raw_socket_fd(&socket) {
            Some(fd) => {
                if wait_io(fd, wake_read) {
                    continue; // commande en attente → re-drain au prochain tour
                }
            }
            None => thread::sleep(Duration::from_millis(4)),
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &wake_read;
            thread::sleep(Duration::from_millis(4));
        }

        // 3. Read CDP messages
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(val) = serde_json::from_str::<Value>(text.as_ref()) else {
                    continue;
                };

                // Check method
                if let Some(method) = val.get("method").and_then(Value::as_str) {
                    match method {
                        "Page.screencastFrame" => {
                            if let Some(params) = val.get("params") {
                                let session_id = params.get("sessionId").and_then(Value::as_u64);
                                if let Some(session_id) = session_id {
                                    // backpressure : l'UI acquitte en consommant
                                    // (AckFrame). Une nouvelle frame avant l'ack →
                                    // l'ancienne est perdue (ack immédiat, Chrome
                                    // continue de produire).
                                    if let Some(stale) = pending_frame.replace(session_id) {
                                        let _ = socket.send(Message::Text(
                                            cdp::screencast_frame_ack(next_id, stale),
                                        ));
                                        next_id += 1;
                                    }
                                }

                                if let Some(base64_data) =
                                    params.get("data").and_then(Value::as_str)
                                    && let Ok(bytes) = STANDARD.decode(base64_data)
                                    && let Ok(dyn_image) = image::load_from_memory_with_format(
                                        &bytes,
                                        image::ImageFormat::Jpeg,
                                    )
                                {
                                    let rgba = dyn_image.into_rgba8();
                                    let render_img = gpui_render_image(rgba);
                                    push_event_lossy(
                                        events,
                                        wake,
                                        BrowserEvent::Page {
                                            image: render_img,
                                            title: page_title.clone(),
                                            url: current_url.clone(),
                                            session_id: session_id.unwrap_or(0),
                                        },
                                    );
                                }
                            }
                        }
                        "Page.navigatedWithinDocument" | "Page.frameNavigated" => {
                            // Le UrlTracker filtre les iframes ; l'URL vient de
                            // params.url (navigatedWithinDocument) ou params.frame.url.
                            if let Some(params) = val.get("params")
                                && let Some(url) = url_tracker.on_nav_event(method, params)
                            {
                                *current_url = url;
                                // rafraîchit les états back/forward après chaque navigation
                                nav_history_request_id = Some(next_id);
                                nav_history_step = 0;
                                socket
                                    .send(Message::Text(cdp::get_navigation_history(next_id)))
                                    .map_err(|e| e.to_string())?;
                                next_id += 1;
                            }
                        }
                        "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
                            if let Some(frame_id) = val
                                .get("params")
                                .and_then(|p| p.get("frameId"))
                                .and_then(Value::as_str)
                                && let Some(loading) =
                                    url_tracker.on_loading_event(method, frame_id)
                            {
                                push_event_lossy(events, wake, BrowserEvent::Loading(loading));
                                if method == "Page.frameStoppedLoading" {
                                    title_request_id = Some(next_id);
                                    let _ = socket.send(Message::Text(cdp::evaluate(
                                        next_id,
                                        "document.title",
                                    )));
                                    next_id += 1;
                                }
                            }
                        }
                        "Page.javascriptDialogOpening" => {
                            if let Some(params) = val.get("params") {
                                let message =
                                    params.get("message").and_then(Value::as_str).unwrap_or("");
                                let prompt = params
                                    .get("defaultPrompt")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                // auto-accept : sinon la page reste bloquée sur alert()
                                dialog_request_id = Some(next_id);
                                socket
                                    .send(Message::Text(cdp::handle_java_script_dialog(
                                        next_id,
                                        true,
                                        prompt.as_deref(),
                                    )))
                                    .map_err(|e| e.to_string())?;
                                next_id += 1;
                                push_event(
                                    events,
                                    wake,
                                    BrowserEvent::Console(ConsoleLog {
                                        level: "dialog".into(),
                                        text: format!("{message} (accepté automatiquement)"),
                                    }),
                                );
                            }
                        }
                        "Target.attachedToTarget" => {
                            if let Some(target_info) = val.pointer("/params/targetInfo") {
                                let is_page =
                                    target_info.get("type").and_then(Value::as_str) == Some("page");
                                let has_opener = target_info.get("openerId").is_some();
                                // popup ouvert par la page → on y navigue dans la vue courante
                                if is_page
                                    && has_opener
                                    && let Some(url) =
                                        target_info.get("url").and_then(Value::as_str)
                                {
                                    *current_url = url.to_owned();
                                    push_event_lossy(events, wake, BrowserEvent::Loading(true));
                                    socket
                                        .send(Message::Text(cdp::page_navigate(next_id, url)))
                                        .map_err(|e| e.to_string())?;
                                    navigate_request_ids.insert(next_id);
                                    next_id += 1;
                                }
                            }
                        }
                        "Runtime.consoleAPICalled" => {
                            if let Some(params) = val.get("params") {
                                let level = params
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("log")
                                    .to_owned();
                                let mut text_parts = Vec::new();
                                if let Some(args) = params.get("args").and_then(Value::as_array) {
                                    for arg in args {
                                        if let Some(val) = arg.get("value") {
                                            text_parts.push(val.to_string());
                                        } else if let Some(desc) =
                                            arg.get("description").and_then(Value::as_str)
                                        {
                                            text_parts.push(desc.to_owned());
                                        }
                                    }
                                }
                                push_event(
                                    events,
                                    wake,
                                    BrowserEvent::Console(ConsoleLog {
                                        level,
                                        text: text_parts.join(" "),
                                    }),
                                );
                            }
                        }
                        "Runtime.exceptionThrown" => {
                            if let Some(params) = val.get("params") {
                                let desc = params
                                    .get("exceptionDetails")
                                    .and_then(|e| e.get("text"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("Uncaught exception")
                                    .to_owned();
                                push_event(
                                    events,
                                    wake,
                                    BrowserEvent::Console(ConsoleLog {
                                        level: "error".into(),
                                        text: desc,
                                    }),
                                );
                            }
                        }
                        "Browser.downloadWillBegin" => {
                            if let Some(params) = val.get("params")
                                && let Some(id) = params.get("guid").and_then(Value::as_str)
                            {
                                let filename = params
                                    .get("suggestedFilename")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let url = params.get("url").and_then(Value::as_str).unwrap_or("");
                                push_event(
                                    events,
                                    wake,
                                    BrowserEvent::DownloadStart {
                                        id: id.to_owned(),
                                        filename: filename.to_owned(),
                                        url: url.to_owned(),
                                    },
                                );
                            }
                        }
                        "Browser.downloadProgress" => {
                            if let Some(params) = val.get("params")
                                && let Some(id) = params.get("guid").and_then(Value::as_str)
                            {
                                let received = params
                                    .get("receivedBytes")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0);
                                let total = params
                                    .get("totalBytes")
                                    .and_then(Value::as_i64)
                                    .filter(|t| *t >= 0)
                                    .map(|t| t as u64);
                                let state = params
                                    .get("state")
                                    .and_then(Value::as_str)
                                    .and_then(downloads::DownloadState::from_cdp);
                                match state {
                                    Some(downloads::DownloadState::InProgress) => {
                                        // progression throttlée par guid
                                        let now = Instant::now();
                                        let send =
                                            download_progress_last.get(id).is_none_or(|last| {
                                                now.duration_since(*last)
                                                    > Duration::from_millis(250)
                                            });
                                        if send {
                                            download_progress_last.insert(id.to_owned(), now);
                                            push_event_lossy(
                                                events,
                                                wake,
                                                BrowserEvent::DownloadProgress {
                                                    id: id.to_owned(),
                                                    received,
                                                    total,
                                                },
                                            );
                                        }
                                    }
                                    Some(state) => {
                                        download_progress_last.remove(id);
                                        let path = params
                                            .get("filePath")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned);
                                        push_event(
                                            events,
                                            wake,
                                            BrowserEvent::DownloadDone {
                                                id: id.to_owned(),
                                                ok: state == downloads::DownloadState::Completed,
                                                path,
                                            },
                                        );
                                    }
                                    None => {}
                                }
                            }
                        }
                        "Network.requestWillBeSent" => {
                            if let Some(params) = val.get("params")
                                && let Some(id) = params.get("requestId").and_then(Value::as_str)
                                && let Some(request) = params.get("request")
                            {
                                let url = request.get("url").and_then(Value::as_str).unwrap_or("");
                                let method =
                                    request.get("method").and_then(Value::as_str).unwrap_or("");
                                push_event_lossy(
                                    events,
                                    wake,
                                    BrowserEvent::NetworkStart {
                                        id: id.to_owned(),
                                        url: url.to_owned(),
                                        method: method.to_owned(),
                                    },
                                );
                            }
                        }
                        "Network.responseReceived" => {
                            if let Some(params) = val.get("params")
                                && let Some(id) = params.get("requestId").and_then(Value::as_str)
                                && let Some(status) =
                                    params.pointer("/response/status").and_then(Value::as_u64)
                            {
                                push_event_lossy(
                                    events,
                                    wake,
                                    BrowserEvent::NetworkResponded {
                                        id: id.to_owned(),
                                        status: status as u16,
                                    },
                                );
                            }
                        }
                        "Network.loadingFailed" => {
                            if let Some(params) = val.get("params")
                                && let Some(id) = params.get("requestId").and_then(Value::as_str)
                            {
                                let error = params
                                    .get("errorText")
                                    .and_then(Value::as_str)
                                    .unwrap_or("network error");
                                let canceled = params
                                    .get("canceled")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                push_event_lossy(
                                    events,
                                    wake,
                                    BrowserEvent::NetworkFailed {
                                        id: id.to_owned(),
                                        error: error.to_owned(),
                                        canceled,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }

                // Check responses to requests
                if let Some(resp_id) = val.get("id").and_then(Value::as_u64) {
                    if navigate_request_ids.remove(&resp_id) {
                        if let Some(error_text) = val
                            .get("result")
                            .and_then(|r| r.get("errorText"))
                            .and_then(Value::as_str)
                            && !error_text.is_empty()
                        {
                            push_event(
                                events,
                                wake,
                                BrowserEvent::Failed(navigate_error_message(
                                    error_text,
                                    current_url,
                                )),
                            );
                        }
                    } else if dialog_request_id == Some(resp_id) {
                        dialog_request_id = None;
                    } else if nav_history_request_id == Some(resp_id) {
                        nav_history_request_id = None;
                        if let Some(result) = val.get("result") {
                            let index = result
                                .get("currentIndex")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            let entries = result.get("entries").and_then(Value::as_array);
                            if let Some(entries) = entries {
                                let can_back = index > 0;
                                let can_forward = (index as usize) + 1 < entries.len();
                                push_event_lossy(
                                    events,
                                    wake,
                                    BrowserEvent::NavHistory {
                                        can_back,
                                        can_forward,
                                    },
                                );
                                if nav_history_step != 0 {
                                    let target = index + nav_history_step as i64;
                                    if let Some(entry) = entries.get(target.unsigned_abs() as usize)
                                        && let Some(entry_id) =
                                            entry.get("id").and_then(Value::as_u64)
                                    {
                                        socket
                                            .send(Message::Text(cdp::navigate_to_history_entry(
                                                next_id, entry_id,
                                            )))
                                            .map_err(|e| e.to_string())?;
                                        next_id += 1;
                                    }
                                    nav_history_step = 0;
                                }
                            }
                        }
                    }
                    if screenshot_request_id == Some(resp_id) {
                        screenshot_request_id = None;
                        if let Some(base64_data) = val
                            .get("result")
                            .and_then(|r| r.get("data"))
                            .and_then(Value::as_str)
                            && let Ok(bytes) = STANDARD.decode(base64_data)
                        {
                            push_event(events, wake, BrowserEvent::Screenshot(bytes));
                        }
                    } else if title_request_id == Some(resp_id) {
                        title_request_id = None;
                        if let Some(title) = val
                            .get("result")
                            .and_then(|r| r.get("result"))
                            .and_then(|res| res.get("value"))
                            .and_then(Value::as_str)
                        {
                            page_title = title.to_owned();
                            push_event_lossy(events, wake, BrowserEvent::Title(page_title.clone()));
                        }
                    } else if overlay_request_id == Some(resp_id) {
                        overlay_request_id = None;
                        if let Some(value) = val
                            .get("result")
                            .and_then(|r| r.get("result"))
                            .and_then(|res| res.get("value"))
                            && let Ok(elements) =
                                serde_json::from_value::<Vec<OverlayElement>>(value.clone())
                        {
                            push_event(events, wake, BrowserEvent::Elements(elements));
                        }
                    } else if probe_ids.remove(&resp_id) {
                        // `null` côté page → plus d'élément survolé.
                        let hover = val
                            .pointer("/result/result/value")
                            .and_then(|v| {
                                serde_json::from_value::<Option<ElementHover>>(v.clone()).ok()
                            })
                            .flatten();
                        push_event_lossy(events, wake, BrowserEvent::ElementHover(hover));
                    } else if describe_id == Some(resp_id) {
                        describe_id = None;
                        if let Some(value) = val
                            .get("result")
                            .and_then(|r| r.get("result"))
                            .and_then(|res| res.get("value"))
                            && let Ok(context) =
                                serde_json::from_value::<ElementContext>(value.clone())
                        {
                            push_event(
                                events,
                                wake,
                                BrowserEvent::ElementContext(Box::new(context)),
                            );
                        }
                    } else if find_id == Some(resp_id) {
                        find_id = None;
                        // `{count, index}` produit par find_in_page_script ;
                        // toute autre forme (erreur JS, page sans CSS.highlights)
                        // est ignorée — le compteur reste à son état précédent.
                        if let Some(value) = val.pointer("/result/result/value")
                            && let Ok(result) =
                                serde_json::from_value::<FindPageResult>(value.clone())
                        {
                            push_event(
                                events,
                                wake,
                                BrowserEvent::FindResult {
                                    count: result.count,
                                    index: result.index,
                                },
                            );
                        }
                    } else if eval_request_ids.remove(&resp_id) {
                        let text = if let Some(err) = val.get("error") {
                            format!("Error: {err}")
                        } else if let Some(res) = val.get("result").and_then(|r| r.get("result")) {
                            res.get("value")
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| res.to_string())
                        } else {
                            "undefined".into()
                        };
                        push_event(
                            events,
                            wake,
                            BrowserEvent::Console(ConsoleLog {
                                level: "result".into(),
                                text,
                            }),
                        );
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = socket.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Ok(SessionResult::Shutdown),
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(4));
            }
            Err(e) => {
                return Err(format!("CDP connection error: {e}"));
            }
            _ => {}
        }
    }
}

fn set_socket_nonblocking(socket: &mut WsStream, nonblocking: bool) -> Result<(), String> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream
            .set_nonblocking(nonblocking)
            .map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn raw_socket_fd(socket: &WsStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => Some(stream.as_raw_fd()),
        _ => None,
    }
}

/// Bloque jusqu'à activité sur le socket CDP ou le self-pipe de commandes.
/// Renvoie true si le pipe a réveillé (des commandes attendent). Zéro réveil
/// à vide : le thread dort dans poll(2), pas dans une boucle à 4 ms.
#[cfg(target_os = "linux")]
fn wait_io(socket_fd: i32, wake_fd: Option<i32>) -> bool {
    let mut fds = [
        libc::pollfd {
            fd: socket_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let count = if wake_fd.is_some() { 2 } else { 1 };
    loop {
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), count, -1) };
        if ready < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break;
    }
    let pipe_fired = fds[1].revents & libc::POLLIN != 0;
    if pipe_fired {
        // vide le pipe (sinon poll re-rentre immédiatement en boucle)
        if let Some(fd) = wake_fd {
            let mut buf = [0u8; 64];
            unsafe { while libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) > 0 {} }
        }
    }
    pipe_fired
}

/// Métriques + redémarrage du screencast aux nouvelles dimensions.
fn send_screencast_restart(
    socket: &mut WsStream,
    next_id: &mut u64,
    width: u32,
    height: u32,
    scale: f32,
    mobile: bool,
) -> Result<(), String> {
    socket
        .send(Message::Text(cdp::set_device_metrics(
            *next_id, width, height, scale, mobile,
        )))
        .map_err(|e| e.to_string())?;
    *next_id += 1;

    let _ = socket.send(Message::Text(cdp::stop_screencast(*next_id)));
    *next_id += 1;

    let physical_w = ((width as f32) * scale).round() as u32;
    let physical_h = ((height as f32) * scale).round() as u32;
    socket
        .send(Message::Text(cdp::start_screencast(
            *next_id,
            physical_w.max(width),
            physical_h.max(height),
            "jpeg",
            90,
        )))
        .map_err(|e| e.to_string())?;
    *next_id += 1;
    Ok(())
}
