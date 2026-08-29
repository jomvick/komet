//! Permission bridge end-to-end (Paseo parity, plan Task 4):
//!
//! - `ask_blocks_until_permit`: a harness blocked on
//!   `RunControls.request_permission` parks the run with a `Permission`
//!   part in the transcript; the user's Allow resolves the oneshot and the
//!   turn completes with the payload.
//! - `deny_stops_agent`: Deny resolves the bridge AND interrupts the run
//!   (`respond_permission` deny→interrupt) — the agent must never reach its
//!   post-deny output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;

use komet_doc::{MessagePart, MessageStatus, SessionMessageEntry};
use komet_engine::{EngineCore, HarnessRegistry};
use komet_harness::{Harness, HarnessError, RunControls};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, PermissionChoice, PermissionKind, ReasoningLevel,
    RunRequest, SandboxLevel, SessionStatus, SteeringMode,
};

const CHAT: &str = "chat-perm";
const MAIN_PROMPT: &str = "list the workspace";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
        permission_timeout_ms: None,
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("hs-perm".into()),
        reason: None,
    }
}

fn session_started() -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-perm".into(),
        assistant_message_id: "a-perm".into(),
    }
}

fn text(t: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: t.into() }
}

/// Asks permission through the bridge before doing anything. Serves only the
/// test's own dispatch (the auto-titler also runs this harness and completes
/// instantly). After an Allow the payload streams; after a Deny the agent
/// "tries" to continue, but the engine's deny→interrupt must cut it first.
struct PermitHarness {
    payload: &'static str,
}

#[async_trait]
impl Harness for PermitHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Permit"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let serve_main = request.prompt == MAIN_PROMPT;
        let payload = self.payload;
        let kind = PermissionKind::Command {
            cmdline: "ls -la".into(),
        };
        let summary = "Run `ls -la` (Sandboxed — approval required)".to_string();
        let choices = vec![
            PermissionChoice::Allow,
            PermissionChoice::AllowAlways {
                scope: komet_proto::Scope::Chat,
            },
            PermissionChoice::Deny,
        ];
        tokio::spawn(async move {
            if !serve_main {
                // The auto-titler's side run: complete instantly.
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
                return;
            }
            let _ = tx.send(Ok(session_started())).await;
            let resolved = (controls.request_permission)(kind, summary, choices).await;
            match resolved {
                Ok(PermissionChoice::Deny) | Err(_) => {
                    // The agent ignores the denial and reaches for its next
                    // command — only the engine's deny→interrupt stops it.
                    tokio::select! {
                        _ = controls.interrupt.cancelled() => {
                            let _ = tx.send(Ok(done(DoneStatus::Interrupted))).await;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {
                            let _ = tx.send(Ok(text("RAN AFTER DENY"))).await;
                            let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
                        }
                    }
                }
                Ok(_) => {
                    let _ = tx.send(Ok(text(payload))).await;
                    let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
                }
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn assemble() -> (EngineCore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(PermitHarness {
        payload: "listed the workspace",
    }));
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    (core, dir)
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn permission_request_id(core: &EngineCore) -> Option<String> {
    entries(core).iter().find_map(|e| {
        e.parts.iter().find_map(|p| match p {
            MessagePart::Permission {
                request_id,
                resolved: false,
                ..
            } => Some(request_id.clone()),
            _ => None,
        })
    })
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_blocks_until_permit() {
    let (core, _dir) = assemble();
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request(MAIN_PROMPT), None)
        .await
        .expect("dispatch");

    // The bridge surfaces a Permission part and the run stays live (Working)
    // — blocked, not settled.
    wait_for(
        || permission_request_id(&core).is_some(),
        "permission part in doc",
    )
    .await;
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Working),
        "the run must still be blocked on the permit"
    );
    assert!(
        !entries(&core)
            .iter()
            .any(|e| e.parts.iter().any(|p| matches!(
                p,
                MessagePart::Text { text, .. } if text == "listed the workspace"
            ))),
        "no output may land before the permit"
    );

    // The user allows; the blocked turn completes.
    let request_id = permission_request_id(&core).unwrap();
    let delivered = core
        .sessions
        .respond_permission(CHAT, &request_id, PermissionChoice::Allow)
        .expect("respond_permission");
    assert!(delivered, "the permit must reach the live run");
    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts.iter().any(|p| {
                        matches!(p, MessagePart::Text { text, .. } if text == "listed the workspace")
                    })
            })
        },
        "allowed turn to complete",
    )
    .await;
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to settle idle",
    )
    .await;
    core.sessions.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_stops_agent() {
    let (core, _dir) = assemble();
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request(MAIN_PROMPT), None)
        .await
        .expect("dispatch");

    wait_for(
        || permission_request_id(&core).is_some(),
        "permission part in doc",
    )
    .await;
    let request_id = permission_request_id(&core).unwrap();
    let delivered = core
        .sessions
        .respond_permission(CHAT, &request_id, PermissionChoice::Deny)
        .expect("respond_permission");
    assert!(delivered, "the denial must reach the live run");

    // Deny interrupts the run: it settles WITHOUT the agent's next output
    // ever landing ("RAN AFTER DENY" would mean the deny→interrupt failed).
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "denied run to settle",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !entries(&core)
            .iter()
            .any(|e| e.parts.iter().any(|p| matches!(
                p,
                MessagePart::Text { text, .. } if text == "RAN AFTER DENY"
            ))),
        "the agent must not continue after a denial"
    );
    core.sessions.shutdown().await;

#[tokio::test(flavor = "multi_thread")]
async fn permission_timeout_auto_denies_and_resolves() {
    // D4 — the hang guard: a runtime permission request left unanswered past
    // the timeout is auto-DENIED (Deny interrupts the run) and the doc's
    // `Permission` part is resolved, so a dead approval panel never renders.
    let (core, _dir) = assemble();
    core.sessions
        .set_permission_timeout(Some(Duration::from_millis(150)));
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request(MAIN_PROMPT), None)
        .await
        .expect("dispatch");

    wait_for(
        || permission_request_id(&core).is_some(),
        "permission part in doc",
    )
    .await;
    let request_id = permission_request_id(&core);
    assert!(request_id.is_some(), "permission part was created");

    // The engine must NOT output the agent's next command: the unanswered
    // request auto-denies → the run interrupts → it settles idle. (The
    // PermitHarness would otherwise sit in `tokio::select!` for 3s.)
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "hung permission run to settle after timeout auto-deny",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !entries(&core)
            .iter()
            .any(|e| e.parts.iter().any(|p| matches!(
                p,
                MessagePart::Text { text, .. } if text == "RAN AFTER DENY"
            ))),
        "the agent must not continue after a timeout auto-deny"
    );
    let delivered = core
        .sessions
        .respond_permission(CHAT, request_id.as_ref().unwrap(), PermissionChoice::Allow);
    assert_eq!(
        delivered.expect("respond_permission call"),
        false,
        "an auto-denied request must not be re-resolvable (already settled)"
    );
    core.sessions.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_timeout_disabled_never_interrupts() {
    // With the guard disabled (`None`), an unanswered request must hang forever
    // — the proven blocking semantics stay intact for attended sessions.
    let (core, _dir) = assemble();
    core.sessions.set_permission_timeout(None);
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request(MAIN_PROMPT), None)
        .await
        .expect("dispatch");

    wait_for(
        || permission_request_id(&core).is_some(),
        "permission part in doc",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Still waiting on the harness (no auto-deny), the session is Working.
    assert_ne!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle),
        "a disabled guard must not interrupt an unanswered permission"
    );
    core.sessions.shutdown().await;
}
}
