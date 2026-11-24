//! Exécuteur UI du pont agent → Browser visible (spec 2026-11-24, Phase 3).
//!
//! Une seule boucle par shell : elle draine les jobs du moteur
//! (`PollBrowserJobs`) pendant qu'une surface Browser est ouverte, les exécute
//! sur la surface du panneau courant, et repousse chaque résultat
//! (`BrowserJobResult`). Pas de surface ouverte → pas de poll (zéro trafic,
//! les outils répondent alors leur erreur explicite côté moteur).

use std::time::Duration;

use komet_rpc::methods;

use crate::browser::{self, OverlayElement};

use super::Shell;

/// Intervalle de poll pendant qu'un Browser est ouvert.
const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// Attente max d'un snapshot / d'une capture, par pas de poll.
const AWAIT_TIMEOUT: Duration = Duration::from_millis(4000);
const AWAIT_STEP: Duration = Duration::from_millis(60);

/// Démarre la boucle (une seule ; appelée une fois à la création du shell —
/// la tâche vit tant que le shell vit, donc `WeakEntity` suffit).
pub(super) fn start_bridge(cx: &mut gpui::Context<Shell>) {
    cx.spawn(async move |this: gpui::WeakEntity<Shell>, cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            // Une surface ouverte + un client RPC, sinon zéro trafic.
            let Ok(target) = this.update(cx, |shell, cx| {
                let surface = shell.current_browser_surface(cx)?;
                let engine = shell.state.read(cx).engine().cloned()?;
                Some((surface, engine.client_arc()))
            }) else {
                return; // shell parti
            };
            let Some((surface, client)) = target else {
                continue;
            };
            // Poll asynchrone : jamais de blocage du thread UI.
            let jobs = match client
                .call(methods::POLL_BROWSER_JOBS, serde_json::json!({}))
                .await
            {
                Ok(reply) => {
                    serde_json::from_value::<komet_engine::browser_bridge::JobsReply>(reply)
                        .map(|parsed| parsed.jobs)
                        .unwrap_or_default()
                }
                Err(_) => continue, // moteur joignable plus tard
            };
            for job in jobs {
                let result = execute_browser_job(&surface, &job, cx).await;
                let (ok, value) = match result {
                    Ok(value) => (true, value),
                    Err(error) => (false, serde_json::json!({ "error": error })),
                };
                let frame = komet_engine::browser_bridge::BrowserJobResultFrame {
                    job_id: job.id.clone(),
                    ok,
                    value,
                };
                let _ = client
                    .call(
                        methods::BROWSER_JOB_RESULT,
                        serde_json::to_value(&frame).expect("serializable frame"),
                    )
                    .await;
            }
        }
    })
    .detach();
}

impl Shell {
    /// La surface qui porte les jobs agent : celle du panneau courant, sinon
    /// n'importe laquelle ouverte (un seul navigateur visible à la fois en
    /// pratique).
    fn current_browser_surface(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Entity<crate::browser::BrowserSurface>> {
        let key = self.panel_key(cx);
        self.browsers
            .get(&key)
            .cloned()
            .or_else(|| self.browsers.values().next().cloned())
    }
}

/// Exécution d'un job sur la surface Browser du panneau courant. Toutes les
/// erreurs sont des messages verbatim pour l'agent.
async fn execute_browser_job(
    surface: &gpui::Entity<browser::BrowserSurface>,
    job: &komet_engine::browser_bridge::BrowserJob,
    cx: &mut gpui::AsyncApp,
) -> Result<serde_json::Value, String> {
    let tool = job.tool.as_str();
    match tool {
        "browser_navigate" => {
            let url = job
                .args
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing argument: url")?;
            surface.update(cx, |surface, cx| {
                surface
                    .agent_navigate(url, cx)
                    .map(|_| serde_json::json!({ "url": surface.agent_url() }))
            })
        }
        "browser_snapshot" => {
            // nouvelle extraction : on attend une génération plus récente
            let before = surface.update(cx, |surface, _| surface.agent_elements_seq());
            surface.update(cx, |surface, cx| surface.agent_request_elements(cx));
            wait_for(
                cx,
                surface.clone(),
                |surface| surface.agent_elements_seq() > before,
                AWAIT_TIMEOUT,
            )
            .await?;
            snapshot_value(surface, cx)
        }
        "browser_click" => {
            let uid = uid_arg(&job.args)?;
            surface.update(cx, |surface, cx| {
                surface
                    .agent_click(uid, cx)
                    .map(|_| serde_json::json!({ "clicked": uid }))
            })
        }
        "browser_fill" => {
            let uid = uid_arg(&job.args)?;
            let text = job
                .args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing argument: text")?
                .to_owned();
            surface.update(cx, |surface, cx| {
                surface
                    .agent_fill(uid, &text, cx)
                    .map(|_| serde_json::json!({ "filled": uid }))
            })
        }
        "browser_press_key" => {
            let key = job
                .args
                .get("key")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing argument: key")?
                .to_owned();
            surface.update(cx, |surface, cx| {
                surface
                    .agent_press_key(&key, cx)
                    .map(|_| serde_json::json!({ "key": key }))
            })
        }
        "browser_wait" => {
            let ms = job
                .args
                .get("ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(250)
                .min(10_000);
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            Ok(serde_json::json!({ "waited": ms }))
        }
        "browser_screenshot" => {
            let before = surface.update(cx, |surface, _| surface.agent_screenshot_seq());
            surface.update(cx, |surface, cx| surface.agent_request_screenshot(cx));
            wait_for(
                cx,
                surface.clone(),
                |surface| surface.agent_screenshot_seq() > before,
                AWAIT_TIMEOUT,
            )
            .await?;
            let path = surface.update(cx, |surface, _| surface.agent_screenshot_path());
            match path {
                Some(path) => Ok(serde_json::json!({ "path": path.display().to_string() })),
                None => Err("screenshot could not be saved".into()),
            }
        }
        "browser_console" => {
            let level = job
                .args
                .get("level")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let entries = surface.update(cx, |surface, _| surface.agent_console(level.as_deref()));
            let entries: Vec<serde_json::Value> = entries
                .into_iter()
                .rev()
                .take(100)
                .map(|(level, text)| serde_json::json!({ "level": level, "text": text }))
                .collect();
            Ok(serde_json::json!({ "entries": entries }))
        }
        other => Err(format!("unknown browser tool: {other}")),
    }
}

fn uid_arg(args: &serde_json::Value) -> Result<u32, String> {
    args.get("uid")
        .and_then(serde_json::Value::as_u64)
        .map(|uid| uid as u32)
        .ok_or_else(|| "missing argument: uid".to_string())
}

/// Réponse de snapshot : URL, titre et éléments de la génération courante.
fn snapshot_value(
    surface: &gpui::Entity<browser::BrowserSurface>,
    cx: &mut gpui::AsyncApp,
) -> Result<serde_json::Value, String> {
    surface.update(cx, |surface, _| {
        let elements: Vec<serde_json::Value> = surface
            .agent_elements()
            .into_iter()
            .map(element_json)
            .collect();
        if elements.is_empty() && !surface.agent_has_page() {
            return Err("no page loaded in the visible Browser".to_string());
        }
        Ok(serde_json::json!({
            "url": surface.agent_url(),
            "title": surface.agent_title(),
            "elements": elements,
        }))
    })
}

fn element_json(el: OverlayElement) -> serde_json::Value {
    serde_json::json!({
        "uid": el.uid,
        "role": el.role,
        "text": el.text,
        "tag": el.tag,
        "selector": el.selector,
        "x": el.x,
        "y": el.y,
        "width": el.width,
        "height": el.height,
    })
}

/// Attend qu'un prédicat sur la surface devienne vrai (génération d'éléments,
/// séquence de capture…). Chaque étape réveille gpui pour garder la surface
/// vivante — les événements CDP arrivent par la boucle d'événements.
async fn wait_for(
    cx: &mut gpui::AsyncApp,
    surface: gpui::Entity<browser::BrowserSurface>,
    predicate: impl Fn(&browser::BrowserSurface) -> bool,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Le handle fort garde la surface vivante : update est infaillible.
        let done = surface.update(cx, |surface, _| predicate(surface));
        if done {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for the Browser surface to answer".into());
        }
        cx.background_executor().timer(AWAIT_STEP).await;
    }
}
