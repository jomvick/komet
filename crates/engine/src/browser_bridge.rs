//! Pont agent → Browser visible : une file de jobs d'outils `browser_*`,
//! vidée par l'exécuteur UI (`PollBrowserJobs`) et complétée par
//! `BrowserJobResult`. Le handler MCP attend le résultat avec un délai borné
//! — jamais de succès silencieux, jamais un Chrome lancé en cachette.
//!
//! Le pont vit côté moteur ; la surface visible (profil humain, transparente
//! pour l'utilisateur) reste l'autorité. Voir
//! `docs/superpowers/specs/2026-11-24-browser-agent-tools-design.md`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Délai max d'attente du résultat par l'outil : au-delà, l'agent reçoit une
/// erreur explicite (surface fermée, exécuteur mort…), jamais un hang.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(30);

/// Un appel d'outil browser en attente d'exécution par la surface visible.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BrowserJob {
    pub id: String,
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Réponse de l'exécuteur (shape RPC `BrowserJobResult`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BrowserJobResultFrame {
    #[serde(rename = "jobId")]
    pub job_id: String,
    pub ok: bool,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// Shape RPC de `PollBrowserJobs`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobsReply {
    #[serde(default)]
    pub jobs: Vec<BrowserJob>,
}

pub struct BrowserBridge {
    seq: AtomicU64,
    pending: Mutex<VecDeque<BrowserJob>>,
    waiters:
        Mutex<HashMap<String, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>>,
    notify: tokio::sync::Notify,
}

impl Default for BrowserBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserBridge {
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(1),
            pending: Mutex::new(VecDeque::new()),
            waiters: Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Exécute un outil via la surface visible : met le job en file et
    /// attend le résultat (`Err` = refus/erreur de la surface, message
    /// verbatim pour l'agent).
    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = format!("job-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiters
            .lock()
            .expect("waiters poisoned")
            .insert(id.clone(), tx);
        self.pending
            .lock()
            .expect("pending poisoned")
            .push_back(BrowserJob {
                id: id.clone(),
                tool: tool.to_owned(),
                args,
            });
        self.notify.notify_waiters();
        match tokio::time::timeout(JOB_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // l'exécuteur a pris le job mais son expéditeur est mort
            Ok(Err(_)) => Err("browser executor dropped the result (surface closed?)".into()),
            Err(_) => {
                self.waiters.lock().expect("waiters poisoned").remove(&id);
                // Le job peut déjà être drainé et en cours d'exécution par
                // la surface : l'erreur ne prétend jamais qu'aucun effet de
                // bord n'a eu lieu.
                Err("timed out — the visible Browser surface may still complete this action".into())
            }
        }
    }

    /// Draine jusqu'à `max` jobs (handler RPC `PollBrowserJobs`). Les jobs
    /// dont le caller a déjà expiré sont écartés ici : l'exécuteur ne peut
    /// pas démarrer une action dont l'agent a déjà reçu un échec. La longueur
    /// est lue sous le même verrou que le drain (pas de course avec un
    /// second poll concurrent).
    pub fn poll(&self, max: usize) -> Vec<BrowserJob> {
        let mut pending = self.pending.lock().expect("pending poisoned");
        let mut kept = VecDeque::new();
        let mut live = Vec::new();
        while let Some(job) = pending.pop_front() {
            if !self.waiter_alive(&job.id) {
                // The caller was aborted or timed out: drop the job and its
                // stale waiter entry instead of leaving them queued forever.
                self.waiters
                    .lock()
                    .expect("waiters poisoned")
                    .remove(&job.id);
                continue;
            }
            if live.len() < max {
                live.push(job);
            } else {
                kept.push_back(job);
            }
        }
        *pending = kept;
        live
    }

    /// Vrai si le caller du job est toujours en attente (pas encore expiré).
    /// A waiter is alive only while its receiver is still held. `contains_key`
    /// is not enough: aborting the caller drops the receiver but leaves the
    /// sender in the map, and `Sender::is_closed()` reports exactly that.
    fn waiter_alive(&self, id: &str) -> bool {
        self.waiters
            .lock()
            .expect("waiters poisoned")
            .get(id)
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Nombre de jobs en attente (diagnostic).
    pub fn pending_len(&self) -> usize {
        self.pending.lock().expect("pending poisoned").len()
    }

    /// Handler RPC `BrowserJobResult` : complète le waiter du job.
    /// Un résultat sans waiter (exécuteur en retard sur un timeout) est
    /// ignoré — le waiter a déjà reçu son erreur.
    pub fn complete(&self, frame: BrowserJobResultFrame) {
        let sender = self
            .waiters
            .lock()
            .expect("waiters poisoned")
            .remove(&frame.job_id);
        if let Some(tx) = sender {
            let result = if frame.ok {
                Ok(frame.value)
            } else {
                Err(frame
                    .value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("browser tool failed")
                    .to_owned())
            };
            let _ = tx.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn call_polls_and_completes() {
        let bridge = Arc::new(BrowserBridge::new());
        let caller = {
            let bridge = bridge.clone();
            tokio::spawn(
                async move { bridge.call("browser_snapshot", serde_json::json!({})).await },
            )
        };
        // le job apparaît dans le poll
        tokio::time::sleep(Duration::from_millis(50)).await;
        let jobs = bridge.poll(8);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].tool, "browser_snapshot");
        // l'exécuteur répond
        bridge.complete(BrowserJobResultFrame {
            job_id: jobs[0].id.clone(),
            ok: true,
            value: serde_json::json!({"elements": []}),
        });
        let result = caller.await.unwrap().unwrap();
        assert_eq!(result["elements"], serde_json::json!([]));
        assert_eq!(bridge.pending_len(), 0);
    }

    #[tokio::test]
    async fn failure_result_becomes_a_verbatim_error() {
        let bridge = Arc::new(BrowserBridge::new());
        let caller = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .call("browser_click", serde_json::json!({"uid": 7}))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let jobs = bridge.poll(8);
        bridge.complete(BrowserJobResultFrame {
            job_id: jobs[0].id.clone(),
            ok: false,
            value: serde_json::json!({"error": "stale snapshot — call browser_snapshot again"}),
        });
        let err = caller.await.unwrap().unwrap_err();
        assert_eq!(err, "stale snapshot — call browser_snapshot again");
    }

    #[tokio::test]
    async fn poll_drops_jobs_whose_caller_timed_out() {
        let bridge = Arc::new(BrowserBridge::new());
        let handle = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .call("browser_click", serde_json::json!({"uid": 1}))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        // le caller meurt (drop du task = waiter perdu)
        handle.abort();
        // Abort is asynchronous: await it so the task (and its waiter) is
        // really dropped before asserting, instead of racing on a sleep.
        let _ = handle.await;
        // le waiter a disparu : le poll ne livre plus le job à l'exécuteur
        assert!(bridge.poll(8).is_empty());
    }

    #[tokio::test]
    async fn poll_respects_max_and_no_waiter_result_is_dropped() {
        let bridge = Arc::new(BrowserBridge::new());
        let caller = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .call("browser_wait", serde_json::json!({"ms": 10}))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        // poll(0) ne draine rien
        assert_eq!(bridge.poll(0).len(), 0);
        assert_eq!(bridge.poll(1).len(), 1);
        // résultat tardif sans waiter : pas de panic
        bridge.complete(BrowserJobResultFrame {
            job_id: "job-999".into(),
            ok: true,
            value: serde_json::json!({}),
        });
        bridge.complete(BrowserJobResultFrame {
            job_id: "job-1".into(),
            ok: true,
            value: serde_json::json!({}),
        });
        assert!(caller.await.unwrap().is_ok());
    }
}
