//! Restart round-trip + harness resume continuity (the "chats forget everything
//! after an app restart" regression): the EMBED assembly (`EngineCore::assemble`)
//! is run twice over one data dir, asserting
//! - chats + transcripts survive a graceful shutdown → relaunch;
//! - the next run in an existing chat carries the chat's stored harness-native
//!   session id as `RunRequest.resume` (engine-owned, komet sessions.ts:736);
//! - a kill -9 style crash recovers the session id from the run journal
//!   (komet recoverDraft, sessions.ts:538-552) and stamps streaming entries
//!   `aborted`;
//! - resume is cwd-scoped (harness session stores are keyed by cwd);
//! - a startup crash retries once with the resume kept, and a helper that is
//!   down hard never tombstones the stored session id;
//! - a steer with no live run after a restart dispatches as a new turn that
//!   still resumes the prior conversation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use komet_doc::{
    CommandBasedOn, InputResponse, MessagePart, MessageRole, MessageStatus, SessionCommandEntry,
    SessionCommandPayload, SessionCommandStatus, SessionDoc, SessionMessageEntry, SubagentStatus,
};
use komet_engine::{EngineCore, HarnessRegistry, RunJournal};
use komet_harness::{Harness, HarnessError, RunControls};
use komet_proto::{
    AgentEvent, CompactionStatus, DoneStatus, HarnessId, Model, PermissionChoice,
    PermissionDecision, PermissionKind, QuestionSource, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode, ToolCall, UserInputAnswer, UserInputQuestion,
};
use komet_sync::DocsStore;

const CHAT: &str = "chat-restart";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

fn run_request(prompt: &str, cwd: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
        mcp: None,
        mcp_external: Vec::new(),
    }
}

/// Records every `RunRequest` it receives (the resume-injection probe). A
/// successful run emits `SessionStarted{session_id}` … `Done{session_id}`;
/// while `fail_starts` is positive, a run dies the way a crashed agent child
/// does — an errored Done before any session starts — decrementing the
/// counter (so a transient spawn blip is one failure, a helper that is down
/// hard is `u32::MAX`).
struct RecordingHarness {
    requests: RequestLog,
    session_id: String,
    fail_starts: Arc<Mutex<u32>>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recording"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        let fail = {
            let mut left = self.fail_starts.lock().expect("fail counter");
            if *left > 0 {
                *left -= 1;
                true
            } else {
                false
            }
        };
        let events: Vec<Result<AgentEvent, HarnessError>> = if fail {
            vec![Ok(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("Recording exited unexpectedly (exit code 1): boom".into()),
                session_id: None,
                reason: None,
            })]
        } else {
            vec![
                Ok(AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: request.cwd.clone(),
                    session_id: self.session_id.clone(),
                    assistant_message_id: "a-1".into(),
                }),
                Ok(AgentEvent::TextDelta {
                    text: format!("ack: {}", request.prompt),
                }),
                Ok(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(self.session_id.clone()),
                    reason: None,
                }),
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

fn assemble(dir: &std::path::Path, harness: RecordingHarness) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(harness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

fn queue_run(core: &EngineCore, prompt: &str, cwd: &str, message_id: &str) {
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run_request(prompt, cwd),
                message_id: message_id.into(),
            },
        )
        .expect("queue run command");
}

async fn wait_for<F>(predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    wait_for_within(predicate, what, Duration::from_secs(10)).await;
}

async fn wait_for_within<F>(mut predicate: F, what: &str, deadline: Duration)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + deadline;
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Tolerant read for hot-polling predicates (mirrors e2e.rs `entries_now`).
fn entries_now(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    entries_now(core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn stored_harness_session(core: &EngineCore) -> Option<(String, Option<String>)> {
    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    chat.harness_session_id
        .map(|id| (id, chat.harness_session_cwd))
}

/// Create + name the chat row up front so the auto-titler (which runs its own
/// harness request after a completed exchange on an UNTITLED chat) stays out
/// of the recorded request log.
fn pre_title(core: &EngineCore) {
    core.workspace
        .create_space("space-restart", &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(CHAT, Some("space-restart"), None, None, None)
        .expect("create chat row");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");
}

/// One full turn in a fresh engine over `dir`, then graceful shutdown — the
/// "before restart" phase shared by the tests below.
async fn run_one_turn_and_shutdown(dir: &std::path::Path, requests: &RequestLog, session: &str) {
    let core = assemble(
        dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: session.into(),
            fail_starts: Default::default(),
        },
    );
    pre_title(&core);
    queue_run(
        &core,
        "remember the codeword PINEAPPLE",
        "/tmp",
        "msg-user-1",
    );
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;
    core.shutdown().await;
    drop(core);
}

#[tokio::test]
async fn restart_roundtrip_restores_chats_transcript_and_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-restart-1").await;
    assert_eq!(
        requests.lock().unwrap()[0].resume,
        None,
        "a chat's first run must start a fresh harness session"
    );

    // Relaunch over the same data dir (the embedded-engine restart path).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-restart-2".into(),
            fail_starts: Default::default(),
        },
    );

    // Sidebar state survived: the chat row is back with its cwd, preview, and
    // the stored harness session (cwd-scoped).
    let chats = core.workspace.read_chats().expect("read chats");
    assert_eq!(chats.len(), 1, "chat row survives restart: {chats:#?}");
    assert_eq!(chats[0].id, CHAT);
    assert_eq!(chats[0].cwd.as_deref(), Some("/tmp"));
    assert!(chats[0].last_message_preview.is_some());
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-1".into(), Some("/tmp".into())))
    );

    // Transcript survived: user + completed assistant entry, texts intact.
    let entries = entries_now(&core);
    assert_eq!(
        entries.len(),
        2,
        "transcript survives restart: {entries:#?}"
    );
    assert_eq!(entries[0].id, "msg-user-1");
    assert_eq!(entries[0].role, MessageRole::User);
    assert_eq!(entries[1].role, MessageRole::Assistant);
    assert_eq!(entries[1].status, Some(MessageStatus::Complete));
    assert!(matches!(
        &entries[1].parts[0],
        MessagePart::Text { text, .. } if text.contains("PINEAPPLE")
    ));

    // The next run resumes the SAME harness conversation: the engine injects
    // the stored session id even though the caller sent `resume: None`.
    queue_run(&core, "what was the codeword?", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete",
    )
    .await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-restart-1"),
            "post-restart dispatch must resume the stored harness session"
        );
    }
    // The fresh turn's session id replaces the stored one.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-2".into(), Some("/tmp".into())))
    );

    // Two turns went through a run that started, waited on the model and
    // streamed — every one of those phase changes was broadcast, and the
    // journal on disk must not have grown a line for any of them (plan
    // Task 10). A replay after a restart that carried a stale "waitingModel"
    // would name a wait that stopped happening minutes ago.
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    let replayed = journal.replay(CHAT, 0).unwrap();
    assert!(
        replayed
            .iter()
            .all(|(_, event)| !matches!(event, AgentEvent::RunPhase { .. })),
        "phase frames are ephemeral: {replayed:#?}"
    );
    assert!(
        replayed.iter().any(|(_, event)| matches!(
            event,
            AgentEvent::SessionStarted { .. }
        )),
        "the durable events around them are still journaled: {replayed:#?}"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn kill_crash_recovers_resume_from_journal_and_stamps_aborted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    // Pin the device id so the manufactured streaming entry counts as OURS.
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();

    // Manufacture the on-disk state a kill -9 mid-run leaves behind:
    // - a chat doc snapshot whose assistant entry is still `streaming`;
    // - a journal whose last event is NOT `Done` (run died mid-stream), holding
    //   the only copy of the harness session id (the debounced workspace-row
    //   write never landed).
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: 1,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial…".into(),
            }],
            created_at: 2,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_starts: Default::default(),
        },
    );
    assert_eq!(core.device_id, "dev-crash");

    // Boot recovery stamped the abandoned streaming entry `aborted` …
    let entries = entries_now(&core);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].status, Some(MessageStatus::Aborted));
    // … and closed the stale journal with a synthetic Done.
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    assert!(matches!(
        journal.last_event(CHAT).unwrap(),
        Some((_, AgentEvent::Done { .. }))
    ));

    // The next run resumes the crashed conversation: the session id was
    // recovered from the journal (its only surviving home).
    pre_title(&core);
    queue_run(&core, "keep going", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "post-crash turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[0].resume.as_deref(),
        Some("hs-crash"),
        "journal-recovered session id must ride the next dispatch"
    );
    core.shutdown().await;
}

/// A harness whose stream stays OPEN after each turn's Done, serving follow-up
/// turns from the steering mailbox — the persistent-session shape (codex; and
/// claude's stream-json stdin). Counts `run()` calls to prove the engine
/// reuses one child across turns instead of respawning.
struct PersistentHarness {
    runs_started: Arc<Mutex<usize>>,
}

#[async_trait]
impl Harness for PersistentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Persistent"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        *self.runs_started.lock().unwrap() += 1;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(32);
        let mut steering = controls.steering;
        tokio::spawn(async move {
            let turn = |n: usize, prompt: &str| {
                vec![
                    AgentEvent::TextDelta {
                        text: format!("turn {n} ack: {prompt}"),
                    },
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some("hs-persist".into()),
                        reason: None,
                    },
                ]
            };
            let first = vec![AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-persist".into(),
                assistant_message_id: "a-1".into(),
            }];
            for ev in first.into_iter().chain(turn(1, "first")) {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            // Parked: serve follow-up turns from the mailbox until the
            // engine hangs up (idle reap / interrupt / shutdown).
            let mut n = 1usize;
            while let Some(steer) = steering.recv().await {
                n += 1;
                let boundary = AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: steer.message_id.map(|id| format!("a-{id}")),
                };
                if tx.send(Ok(boundary)).await.is_err() {
                    return;
                }
                for ev in turn(n, &steer.prompt) {
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
        )
        .boxed())
    }
}

#[tokio::test]
async fn persistent_session_serves_multiple_turns_on_one_child() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let runs_started = Arc::new(Mutex::new(0usize));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(PersistentHarness {
        runs_started: runs_started.clone(),
    }));
    let core = EngineCore::assemble(&dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    pre_title(&core);

    queue_run(&core, "first", "/tmp", "msg-user-1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;

    // The session PARKS (komet runsBySession): the second message routes into
    // the live child instead of spawning a new one.
    queue_run(&core, "second", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete on the same child",
    )
    .await;

    assert_eq!(
        *runs_started.lock().unwrap(),
        1,
        "one harness child must serve both turns"
    );
    let entries = entries_now(&core);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.role == MessageRole::User)
            .count(),
        2
    );
    core.shutdown().await;
}

#[tokio::test]
async fn fresh_crash_auto_resumes_and_notes_the_interruption() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();

    // Same manufactured kill -9 state as above, but FRESH: the streaming entry
    // crashed moments ago, inside the 12h revival window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: now - 60_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial…".into(),
            }],
            created_at: now - 30_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_starts: Default::default(),
        },
    );

    // The run is PICKED BACK UP without any user action (komet: "not just
    // eulogized"): recovery re-dispatches the crashed prompt itself.
    wait_for(
        || complete_assistant_count(&core) == 1,
        "auto-resumed turn to complete",
    )
    .await;

    let entries = entries_now(&core);
    // The aborted entry SAYS why it ended — and that the run is resuming.
    let aborted = entries
        .iter()
        .find(|e| e.status == Some(MessageStatus::Aborted))
        .expect("crashed entry stays, stamped aborted");
    assert!(
        aborted.parts.iter().any(|p| matches!(
            p,
            MessagePart::Error { message, .. }
                if message.contains("engine restart") && message.contains("resuming")
        )),
        "aborted entry carries the visible interruption note"
    );
    // Re-dispatch reuses the original user message id — never a duplicate.
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.role == MessageRole::User)
            .count(),
        1
    );
    // The revived run continues the journal-recovered harness conversation.
    // (An auto-title request may precede it — titling fires at dispatch.)
    let recorded = requests.lock().unwrap().clone();
    let revived = recorded
        .iter()
        .find(|r| r.prompt == "long task")
        .expect("auto-resumed dispatch reached the harness");
    assert_eq!(
        revived.resume.as_deref(),
        Some("hs-crash"),
        "auto-resume must reattach the crashed harness session"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn resume_is_cwd_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-cwd-1").await;

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-cwd-2".into(),
            fail_starts: Default::default(),
        },
    );
    // Same chat, different launch directory: claude session stores are keyed
    // by cwd, so the stored id must NOT be injected.
    queue_run(
        &core,
        "now from another project",
        "/elsewhere",
        "msg-user-2",
    );
    wait_for(
        || complete_assistant_count(&core) == 2,
        "cross-cwd turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[1].resume,
        None,
        "a session created under /tmp must not resume from /elsewhere"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn startup_crash_retries_once_with_resume_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-live").await;

    // Relaunch with a harness whose child dies at startup ONCE (a transient
    // spawn blip). Since the ACP conversion a stale id falls back inside the
    // harness (`session/load` → `session/new`), so a startup death never
    // indicts the stored id: the retry must carry the SAME session id, not
    // start fresh — and never tombstone it.
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-next".into(),
            fail_starts: Arc::new(Mutex::new(1)),
        },
    );
    queue_run(&core, "second turn", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "retried turn to complete",
    )
    .await;

    // The crashed attempt, then exactly one retry — resume kept both times.
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 3, "one crashed attempt + one retry");
        assert_eq!(log[1].resume.as_deref(), Some("hs-live"));
        assert_eq!(
            log[2].resume.as_deref(),
            Some("hs-live"),
            "the retry must keep the stored conversation"
        );
        assert_eq!(log[2].prompt, "second turn");
    }
    // The retry reused the same user entry — no duplicates, no error turn.
    let entries = entries_now(&core);
    let users: Vec<_> = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .collect();
    assert_eq!(users.len(), 2, "retry must not duplicate the user entry");
    assert_eq!(entries.len(), 4, "user+assistant per turn: {entries:#?}");
    // The successful turn's session id replaces the stored one as usual.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-next".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

#[tokio::test]
async fn persistent_startup_crash_keeps_stored_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-live").await;

    // A helper that is down hard: every spawn dies at startup. The old guess
    // logic read this as "bad resume id" and tombstoned a perfectly good
    // session — permanently, surviving restarts (user incident 2026-08-13).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-unused".into(),
            fail_starts: Arc::new(Mutex::new(u32::MAX)),
        },
    );
    queue_run(&core, "second turn", "/tmp", "msg-user-2");
    // Crashed attempt + its single retry — then it must STOP (no spawn loop).
    wait_for(
        || requests.lock().unwrap().len() == 3,
        "crashed attempt and retry to be recorded",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 3, "exactly one retry, no revival loop");
        assert_eq!(log[1].resume.as_deref(), Some("hs-live"));
        assert_eq!(log[2].resume.as_deref(), Some("hs-live"));
    }
    // THE fix: the stored id survives the startup failures — the next send
    // (say, after the restart that heals the helper) resumes the same
    // conversation.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-live".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

/// Real-CLI proof of the whole regression fix: tell claude a codeword, restart
/// the engine (fresh `EngineCore::assemble` over the same data dir), ask for
/// the codeword back — the reply can only contain it if the second run resumed
/// the first run's harness session. Ignored by default: needs an installed,
/// authenticated `claude` CLI and spends real tokens (haiku, two tiny turns).
/// Run with: `cargo test -p komet-engine --test restart_resume -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_remembers_codeword_across_engine_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().to_string();

    let real_request = |prompt: &str| RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: false,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
        mcp: None,
        mcp_external: Vec::new(),
    };
    let assemble_real = || {
        EngineCore::assemble(
            &dir,
            Arc::new(komet_engine::default_registry()),
            HarnessId::ClaudeCode,
            None,
        )
        .expect("engine core assembles")
    };

    let core = assemble_real();
    pre_title(&core); // keep the auto-titler from spending a second model call
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "Remember the codeword: PINEAPPLE. Reply with exactly: stored",
                ),
                message_id: "msg-user-1".into(),
            },
        )
        .expect("queue first real run");
    wait_for_within(
        || complete_assistant_count(&core) == 1,
        "first real claude turn",
        Duration::from_secs(120),
    )
    .await;
    assert!(
        stored_harness_session(&core).is_some(),
        "claude session id must be stored on the chat row"
    );
    core.shutdown().await;
    drop(core);

    // "App restart": a brand-new engine over the same data dir.
    let core = assemble_real();
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "What was the codeword I told you earlier? Reply with just the codeword.",
                ),
                message_id: "msg-user-2".into(),
            },
        )
        .expect("queue second real run");
    wait_for_within(
        || complete_assistant_count(&core) == 2,
        "post-restart real claude turn",
        Duration::from_secs(120),
    )
    .await;

    let entries = entries_now(&core);
    let last_assistant_text: String = entries
        .iter()
        .rev()
        .find(|e| e.role == MessageRole::Assistant)
        .into_iter()
        .flat_map(|e| {
            e.parts.iter().filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        last_assistant_text.to_uppercase().contains("PINEAPPLE"),
        "post-restart reply must recall the codeword (got: {last_assistant_text:?})"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn steer_after_restart_dispatches_new_turn_with_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-steer").await;

    // Relaunch: no live run, no in-process `last_request`. A steer must fall
    // back to a new turn built from the chat's workspace row, resuming the
    // prior harness conversation.
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-steer-2".into(),
            fail_starts: Default::default(),
        },
    );
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "actually, also add tests".into(),
                message_id: Some("msg-user-2".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "steer-as-new-turn to complete",
    )
    .await;

    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].prompt, "actually, also add tests");
        assert_eq!(log[1].cwd, "/tmp", "run config rebuilt from the chat row");
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-steer"),
            "steer-turned-run must resume the stored harness session"
        );
    }
    core.shutdown().await;
}

// ── Task 9 (durabilité) : une réponse survit au run qui l'a posée ──────────

/// A crash long enough ago that boot recovery will NOT auto-resume the turn
/// (the freshness window is 12 h): these tests exercise the durable command
/// path, not the revival path.
const NOT_FRESH_MS: i64 = 1;

fn pin_device(dir: &std::path::Path, device: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("device-id"), device).unwrap();
}

/// Write the on-disk state a kill -9 mid-turn leaves behind: a chat snapshot
/// plus a journal whose last event is not `Done`.
fn write_crash_state(
    dir: &std::path::Path,
    entries: Vec<SessionMessageEntry>,
    events: &[AgentEvent],
) {
    std::fs::create_dir_all(dir).unwrap();
    let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
    let doc = SessionDoc::init(CHAT).unwrap();
    for entry in entries {
        doc.push_message(&entry).unwrap();
    }
    store
        .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
        .unwrap();
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    for event in events {
        journal.append(CHAT, event).unwrap();
    }
}

fn entry_with(
    id: &str,
    role: MessageRole,
    status: MessageStatus,
    device: &str,
    parts: Vec<MessagePart>,
) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts,
        created_at: NOT_FRESH_MS,
        device_id: device.into(),
        status: Some(status),
        continuation_of: None,
    }
}

fn pick_question() -> UserInputQuestion {
    UserInputQuestion {
        id: "q1".into(),
        header: "Pick".into(),
        question: "Which option?".into(),
        options: vec!["Option A".into(), "Option B".into()],
        multi_select: false,
        option_descriptions: Vec::new(),
        allow_free_text: false,
        secret: false,
        default: None,
    }
}

fn crashed_session(session: &str, assistant_id: &str) -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: session.into(),
        assistant_message_id: assistant_id.into(),
    }
}

/// Queue a command under an explicit device id — what a second device's
/// answer/decision looks like once the doc replicates.
fn queue_as(core: &EngineCore, id: &str, device: &str, payload: SessionCommandPayload) {
    let handle = core.doc_host.open(CHAT).expect("open chat");
    let based_on = handle.doc().read_entries().ok().and_then(|entries| {
        entries.last().map(|entry| CommandBasedOn {
            turn_id: Some(entry.id.clone()),
            frontier: None,
        })
    });
    handle
        .doc()
        .queue_command(&SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: device.into(),
            issued_at: chrono::Utc::now().timestamp_millis(),
            based_on,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .expect("queue command");
}

fn command_status(core: &EngineCore, id: &str) -> Option<(SessionCommandStatus, Option<String>)> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_commands()
        .expect("read commands")
        .into_iter()
        .find(|command| command.id == id)
        .map(|command| (command.status, command.resolution))
}

async fn wait_for_command(core: &EngineCore, id: &str) -> (SessionCommandStatus, Option<String>) {
    wait_for(
        || {
            command_status(core, id)
                .is_some_and(|(status, _)| status != SessionCommandStatus::Pending)
        },
        "the command to resolve",
    )
    .await;
    command_status(core, id).expect("command still queued")
}

fn input_part(
    core: &EngineCore,
    request_id: &str,
) -> Option<(bool, Vec<komet_doc::InputResponse>)> {
    entries_now(core).into_iter().find_map(|entry| {
        entry.parts.into_iter().find_map(|part| match part {
            MessagePart::Input {
                request_id: rid,
                resolved,
                responses,
                ..
            } if rid == request_id => Some((resolved, responses)),
            _ => None,
        })
    })
}

type PermissionPart = (bool, Option<PermissionChoice>, Option<String>, Option<i64>);

fn permission_part(core: &EngineCore, request_id: &str) -> Option<PermissionPart> {
    entries_now(core).into_iter().find_map(|entry| {
        entry.parts.into_iter().find_map(|part| match part {
            MessagePart::Permission {
                request_id: rid,
                resolved,
                decision,
                decided_by,
                decided_at,
                ..
            } if rid == request_id => Some((resolved, decision, decided_by, decided_at)),
            _ => None,
        })
    })
}

#[tokio::test]
async fn answering_after_restart_starts_a_new_turn_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    pin_device(&dir, "dev-crash");
    write_crash_state(
        &dir,
        vec![
            entry_with(
                "msg-user-1",
                MessageRole::User,
                MessageStatus::Complete,
                "dev-crash",
                vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "long task".into(),
                }],
            ),
            entry_with(
                "msg-assistant-1",
                MessageRole::Assistant,
                MessageStatus::Streaming,
                "dev-crash",
                vec![
                    MessagePart::Text {
                        id: "t1".into(),
                        text: "thinking…".into(),
                    },
                    MessagePart::Input {
                        id: "in-q-1".into(),
                        request_id: "q-1".into(),
                        questions: vec![pick_question()],
                        resolved: false,
                        responses: Vec::new(),
                        source: QuestionSource::Native,
                    },
                ],
            ),
        ],
        &[crashed_session("hs-q", "msg-assistant-1")],
    );

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-q2".into(),
            fail_starts: Default::default(),
        },
    );
    assert_eq!(core.device_id, "dev-crash");

    // Boot recovery settles the dead turn but leaves the question ANSWERABLE:
    // the UI keeps it up until the user answers (user requirement).
    let entries = entries_now(&core);
    assert_eq!(entries[1].status, Some(MessageStatus::Aborted));
    assert!(
        matches!(
            &entries[1].parts[1],
            MessagePart::Input {
                resolved: false,
                ..
            }
        ),
        "recovery must not cancel an open question: {:?}",
        entries[1].parts[1]
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "an old crashed turn must not auto-resume"
    );

    pre_title(&core);
    let command = core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::RespondInput {
                request_id: "q-1".into(),
                answers: vec![UserInputAnswer {
                    question_id: "q1".into(),
                    labels: vec!["Option B".into()],
                    free_text: None,
                }],
            },
        )
        .expect("queue answer");
    let (status, reason) = wait_for_command(&core, &command).await;
    assert_eq!(status, SessionCommandStatus::Applied);
    assert_eq!(
        reason.as_deref(),
        Some("answered as new turn"),
        "the durable command says what it did"
    );

    wait_for(
        || complete_assistant_count(&core) == 1,
        "the answered turn to complete",
    )
    .await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 1, "one run: the orphan answer's new turn");
        assert!(
            log[0].prompt.contains("Option B"),
            "the answer rides the resumed turn: {}",
            log[0].prompt
        );
        assert_eq!(
            log[0].resume.as_deref(),
            Some("hs-q"),
            "the new turn resumes the crashed harness conversation"
        );
    }
    let (resolved, responses) = input_part(&core, "q-1").expect("question part");
    assert!(resolved);
    assert_eq!(responses.len(), 1);
    assert!(responses[0].applied, "the host delivered this answer");
    assert_eq!(responses[0].device, "dev-crash");
    core.shutdown().await;
}

#[tokio::test]
async fn permission_after_restart_is_recorded_but_run_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    pin_device(&dir, "dev-crash");
    write_crash_state(
        &dir,
        vec![
            entry_with(
                "msg-user-1",
                MessageRole::User,
                MessageStatus::Complete,
                "dev-crash",
                vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "run a command".into(),
                }],
            ),
            entry_with(
                "msg-assistant-1",
                MessageRole::Assistant,
                MessageStatus::Streaming,
                "dev-crash",
                vec![MessagePart::Permission {
                    id: "perm-perm-1".into(),
                    request_id: "perm-1".into(),
                    permission_kind: PermissionKind::Command {
                        cmdline: "cargo test".into(),
                    },
                    summary: "Run cargo test".into(),
                    choices: vec![PermissionChoice::Deny, PermissionChoice::Allow],
                    actions: Vec::new(),
                    resolved: false,
                    decision: None,
                    tool_call_id: None,
                    decided_by: None,
                    decided_at: None,
                    expires_at: None,
                }],
            ),
        ],
        &[crashed_session("hs-perm", "msg-assistant-1")],
    );

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-perm-2".into(),
            fail_starts: Default::default(),
        },
    );

    // The dead approval panel is closed at boot; no decision is invented.
    let (resolved, decision, decided_by, _) =
        permission_part(&core, "perm-1").expect("permission part");
    assert!(resolved);
    assert_eq!(
        decision, None,
        "recovery closes the panel, it decides nothing"
    );
    assert_eq!(decided_by, None);

    // A decision queued offline (or from another device) still lands — and it
    // must not resurrect the run.
    queue_as(
        &core,
        "cmd-permit",
        "dev-phone",
        SessionCommandPayload::Permit {
            request_id: "perm-1".into(),
            decision: PermissionDecision {
                request_id: "perm-1".into(),
                choice: PermissionChoice::Allow,
                selected_action_id: None,
                updated_permissions: None,
            },
        },
    );
    let (status, reason) = wait_for_command(&core, "cmd-permit").await;
    assert_eq!(status, SessionCommandStatus::Applied, "{reason:?}");
    let (resolved, decision, decided_by, decided_at) =
        permission_part(&core, "perm-1").expect("permission part");
    assert!(resolved);
    assert_eq!(decision, Some(PermissionChoice::Allow));
    assert_eq!(decided_by.as_deref(), Some("dev-phone"));
    assert!(decided_at.is_some(), "the deciding device's clock is kept");
    assert!(
        requests.lock().unwrap().is_empty(),
        "recording a decision never dispatches a run"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn concurrent_permission_decisions_keep_deny() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    pin_device(&dir, "dev-crash");
    let permission = |id: &str| MessagePart::Permission {
        id: format!("perm-{id}"),
        request_id: id.into(),
        permission_kind: PermissionKind::Command {
            cmdline: "cargo test".into(),
        },
        summary: "Run cargo test".into(),
        choices: vec![PermissionChoice::Deny, PermissionChoice::Allow],
        actions: Vec::new(),
        resolved: false,
        decision: None,
        tool_call_id: None,
        decided_by: None,
        decided_at: None,
        expires_at: None,
    };
    write_crash_state(
        &dir,
        vec![entry_with(
            "msg-assistant-1",
            MessageRole::Assistant,
            MessageStatus::Streaming,
            "dev-crash",
            vec![permission("perm-a"), permission("perm-b")],
        )],
        &[crashed_session("hs-perm", "msg-assistant-1")],
    );

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-perm-2".into(),
            fail_starts: Default::default(),
        },
    );

    let decide = |id: &str, device: &str, choice: PermissionChoice| {
        queue_as(
            &core,
            id,
            device,
            SessionCommandPayload::Permit {
                request_id: "perm-a".into(),
                decision: PermissionDecision {
                    request_id: "perm-a".into(),
                    choice,
                    selected_action_id: None,
                    updated_permissions: None,
                },
            },
        );
    };
    // Order A: Allow lands first, then a concurrent Deny.
    decide("cmd-a1", "dev-laptop", PermissionChoice::Allow);
    assert_eq!(
        wait_for_command(&core, "cmd-a1").await.0,
        SessionCommandStatus::Applied
    );
    decide("cmd-a2", "dev-phone", PermissionChoice::Deny);
    assert_eq!(
        wait_for_command(&core, "cmd-a2").await.0,
        SessionCommandStatus::Applied
    );
    let (_, decision, decided_by, _) = permission_part(&core, "perm-a").expect("perm-a");
    assert_eq!(
        decision,
        Some(PermissionChoice::Deny),
        "a late Allow must never rewrite what the agent was denied"
    );
    assert_eq!(decided_by.as_deref(), Some("dev-phone"));

    // Same rule in the other order (Deny first, Allow second), on a second id.
    let deny_b = |id: &str, device: &str, choice: PermissionChoice| {
        queue_as(
            &core,
            id,
            device,
            SessionCommandPayload::Permit {
                request_id: "perm-b".into(),
                decision: PermissionDecision {
                    request_id: "perm-b".into(),
                    choice,
                    selected_action_id: None,
                    updated_permissions: None,
                },
            },
        );
    };
    deny_b("cmd-b1", "dev-phone", PermissionChoice::Deny);
    assert_eq!(
        wait_for_command(&core, "cmd-b1").await.0,
        SessionCommandStatus::Applied
    );
    deny_b("cmd-b2", "dev-laptop", PermissionChoice::Allow);
    assert_eq!(
        wait_for_command(&core, "cmd-b2").await.0,
        SessionCommandStatus::Applied
    );
    let (_, decision, decided_by, _) = permission_part(&core, "perm-b").expect("perm-b");
    assert_eq!(decision, Some(PermissionChoice::Deny));
    assert_eq!(decided_by.as_deref(), Some("dev-phone"));
    assert!(
        requests.lock().unwrap().is_empty(),
        "decisions never dispatch runs"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn compaction_metadata_survives_a_viewer_reconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    write_crash_state(
        &dir,
        vec![entry_with(
            "msg-assistant-1",
            MessageRole::Assistant,
            MessageStatus::Complete,
            "dev-host",
            vec![MessagePart::Compaction {
                id: "cmp-c1".into(),
                compaction_id: "c1".into(),
                status: CompactionStatus::Completed,
                trigger: Some("auto".into()),
                tokens_before: Some(165_000),
                tokens_after: None,
                error: None,
                // Journal-only policy: metadata in the doc, prose in the
                // host's journal — never both.
                summary: None,
                summary_bytes: Some(9),
                summary_in_journal: true,
            }],
        )],
        &[
            crashed_session("hs-cmp", "msg-assistant-1"),
            AgentEvent::ContextCompaction {
                id: "c1".into(),
                status: CompactionStatus::Completed,
                trigger: Some("auto".into()),
                tokens_before: Some(165_000),
                tokens_after: None,
                summary: Some("the recap".into()),
                summary_delta: None,
                error: None,
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-cmp".into()),
                reason: None,
            },
        ],
    );

    // A viewer reconnecting (fresh core over the persisted state) sees the
    // cycle's metadata from the doc, on every device.
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-cmp-2".into(),
            fail_starts: Default::default(),
        },
    );
    let part = entries_now(&core)
        .into_iter()
        .flat_map(|entry| entry.parts)
        .find_map(|part| match part {
            MessagePart::Compaction {
                status,
                trigger,
                tokens_before,
                tokens_after,
                summary,
                summary_bytes,
                summary_in_journal,
                ..
            } => Some((
                status,
                trigger,
                tokens_before,
                tokens_after,
                summary,
                summary_bytes,
                summary_in_journal,
            )),
            _ => None,
        })
        .expect("compaction part survived the reconnect");
    assert_eq!(part.0, CompactionStatus::Completed);
    assert_eq!(part.1.as_deref(), Some("auto"));
    assert_eq!(part.2, Some(165_000));
    assert_eq!(part.3, None, "no invented post-compaction reading");
    assert_eq!(part.4, None, "journal-only: the doc carries no prose");
    assert_eq!(part.5, Some(9));
    assert!(part.6);

    // The recap itself is fetched from the host's journal by RPC.
    use komet_rpc::{RpcReply, RpcService as _, methods};
    let reply = core
        .rpc_service()
        .handle(
            methods::GET_COMPACTION_SUMMARY,
            serde_json::json!({ "chatId": CHAT, "compactionId": "c1" }),
        )
        .await
        .expect("compaction summary rpc");
    let RpcReply::Value(reply) = reply else {
        panic!("GetCompactionSummary is a unary reply");
    };
    assert_eq!(reply["status"], "ok", "{reply}");
    assert_eq!(reply["text"], "the recap");
    core.shutdown().await;
}

#[tokio::test]
async fn subagent_duration_is_absent_after_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    pin_device(&dir, "dev-crash");
    write_crash_state(
        &dir,
        vec![entry_with(
            "msg-assistant-1",
            MessageRole::Assistant,
            MessageStatus::Streaming,
            "dev-crash",
            vec![MessagePart::Tool {
                id: "spawn-1".into(),
                call: ToolCall::Unknown {
                    name: "Agent".into(),
                    input: Some(serde_json::json!({ "description": "explore" })),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: Some(SubagentStatus::Running),
                subagent_tail: None,
                subagent_duration_ms: None,
                subagent_tool_count: None,
            }],
        )],
        &[crashed_session("hs-sub", "msg-assistant-1")],
    );

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-sub-2".into(),
            fail_starts: Default::default(),
        },
    );
    let entries = entries_now(&core);
    assert_eq!(entries[0].status, Some(MessageStatus::Aborted));
    let MessagePart::Tool {
        subagent_status,
        subagent_duration_ms,
        subagent_tool_count,
        ..
    } = &entries[0].parts[0]
    else {
        panic!("spawn chip expected, got {:?}", entries[0].parts[0]);
    };
    assert_eq!(
        *subagent_status,
        Some(SubagentStatus::Failed),
        "a child that died with the process is never left 'running'"
    );
    assert_eq!(
        *subagent_duration_ms, None,
        "the journal has no timestamps to rebuild a duration from"
    );
    assert_eq!(*subagent_tool_count, None);
    assert!(requests.lock().unwrap().is_empty());
    core.shutdown().await;
}

/// Task 8/9 boundary: after a restart, an OPEN question on a settled entry is
/// actionable — answering it starts a new turn (see
/// `answering_after_restart_starts_a_new_turn_and_says_so`). An already
/// ANSWERED question is history: a late command for it is recorded as a
/// concurrent answer and must never dispatch a run.
#[tokio::test]
async fn stale_question_is_historical_not_actionable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    pin_device(&dir, "dev-crash");
    let history = MessagePart::Input {
        id: "in-q-old".into(),
        request_id: "q-old".into(),
        questions: vec![pick_question()],
        resolved: true,
        responses: vec![InputResponse {
            answers: vec![UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["Option A".into()],
                free_text: None,
            }],
            decided_at: NOT_FRESH_MS,
            device: "dev-crash".into(),
            applied: true,
        }],
        source: QuestionSource::Native,
    };
    let open = MessagePart::Input {
        id: "in-q-1".into(),
        request_id: "q-1".into(),
        questions: vec![pick_question()],
        resolved: false,
        responses: Vec::new(),
        source: QuestionSource::Native,
    };
    write_crash_state(
        &dir,
        vec![
            entry_with(
                "msg-assistant-0",
                MessageRole::Assistant,
                MessageStatus::Complete,
                "dev-crash",
                vec![history],
            ),
            entry_with(
                "msg-user-1",
                MessageRole::User,
                MessageStatus::Complete,
                "dev-crash",
                vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "second task".into(),
                }],
            ),
            entry_with(
                "msg-assistant-1",
                MessageRole::Assistant,
                MessageStatus::Streaming,
                "dev-crash",
                vec![open],
            ),
        ],
        &[crashed_session("hs-q", "msg-assistant-1")],
    );

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-q2".into(),
            fail_starts: Default::default(),
        },
    );
    pre_title(&core);

    let answer = |label: &str| UserInputAnswer {
        question_id: "q1".into(),
        labels: vec![label.into()],
        free_text: None,
    };

    // The answered question is history: a late answer from another device is
    // recorded as a concurrent answer, and nothing runs.
    queue_as(
        &core,
        "cmd-old",
        "dev-phone",
        SessionCommandPayload::RespondInput {
            request_id: "q-old".into(),
            answers: vec![answer("Option B")],
        },
    );
    let (status, reason) = wait_for_command(&core, "cmd-old").await;
    assert_eq!(status, SessionCommandStatus::Rejected, "{reason:?}");
    assert!(
        reason
            .as_deref()
            .is_some_and(|why| why.contains("recorded, not delivered")),
        "the stale answer is explained, not swallowed: {reason:?}"
    );
    let (resolved, responses) = input_part(&core, "q-old").expect("historical question");
    assert!(resolved);
    assert_eq!(responses.len(), 2, "the late answer is listed, not dropped");
    assert!(responses[0].applied, "the delivered answer keeps its mark");
    assert!(
        !responses[1].applied,
        "the late answer is recorded as a conflict, never delivered"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "a historical question must never start a turn"
    );

    // The open question from the crashed run is actionable: the very same
    // command type now dispatches a new turn.
    let command = core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::RespondInput {
                request_id: "q-1".into(),
                answers: vec![answer("Option B")],
            },
        )
        .expect("queue answer");
    let (status, reason) = wait_for_command(&core, &command).await;
    assert_eq!(status, SessionCommandStatus::Applied);
    assert_eq!(
        reason.as_deref(),
        Some("answered as new turn"),
        "only an actionable question restarts the agent"
    );
    wait_for(
        || complete_assistant_count(&core) == 2,
        "the answered turn to complete",
    )
    .await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 1, "exactly one run: the open question's turn");
        assert!(log[0].prompt.contains("Option B"));
    }
    core.shutdown().await;
}
