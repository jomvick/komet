//! Quiet-turn diagnostics (`KOMET_ACP_QUIET_SETTLE_MS`), tested with the GROK
//! spec so no adapter-specific evidence is in play. Own test binary: the env
//! knob is process-global, and every test here shares the one value.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use komet_proto::{
    AgentEvent, DoneReason, DoneStatus, RunRequest, SandboxLevel, UserInputAnswer,
    UserInputQuestion,
};

const QUIET_MS: u64 = 1200;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set before any harness runs in this test process; all
        // tests in this binary share the one value.
        unsafe { std::env::set_var("KOMET_ACP_QUIET_SETTLE_MS", QUIET_MS.to_string()) };
    });
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("grok-4.5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        use_mcp: false,
        worktree: None,
        resume: None,
        mcp: None,
        mcp_external: Vec::new(),
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions: Vec<UserInputQuestion>, _source| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                    free_text: None,
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        request_permission: RunControls::noop_permission(),
        steering: steer_rx,
        interrupt: token.clone(),
        env: Vec::new(),
    };
    (controls, steer_tx, token)
}

async fn run_and_collect(
    harness: AcpHarness,
    prompt: &str,
    timeout: Duration,
) -> Vec<(std::time::Instant, AgentEvent)> {
    let (controls, _steer, _token) = controls();
    let harness = harness.with_executable(fixture_path());
    let stream = harness
        .run(request(prompt), controls)
        .await
        .expect("run starts");
    tokio::time::timeout(timeout, async move {
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push((std::time::Instant::now(), ev.expect("stream event")));
        }
        events
    })
    .await
    .expect("run finished in time")
}

fn dones(events: &[(std::time::Instant, AgentEvent)]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

fn done_reasons(events: &[(std::time::Instant, AgentEvent)]) -> Vec<Option<DoneReason>> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::Done { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

/// A generic agent whose prompt response is dropped: content streamed, no
/// open tool, then silence. Quiet is not proof of completion; the harness
/// keeps the future alive and reports the missing response when the child exits.
#[tokio::test]
async fn generic_dropped_reply_is_not_settled_off_the_quiet_window() {
    init_env();
    let started = std::time::Instant::now();
    let events = run_and_collect(
        AcpHarness::grok(),
        "scenario:quiet-starve",
        Duration::from_secs(20),
    )
    .await;
    let done = events
        .iter()
        .find(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .expect("child exit must close the unresolved prompt");
    assert!(
        matches!(
            &done.1,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            }
        ),
        "{events:?}"
    );
    let done_at = done.0.duration_since(started);
    // The diagnostic window is 1.2s; the fixture stays alive for 8s. No
    // synthetic Done may be emitted just because the stream went quiet.
    assert!(
        done_at > Duration::from_secs(6),
        "Done at {done_at:?} — should wait for child exit, not the \
         {QUIET_MS}ms quiet window"
    );
    assert_eq!(dones(&events).len(), 1, "{events:?}");
    assert_eq!(done_reasons(&events), vec![Some(DoneReason::ChildExit)]);
}

/// A resolved tool is followed by a quiet reasoning interval longer than the
/// diagnostic window, then a late response to the original prompt. It must
/// remain the same turn and settle exactly once from that real response.
#[tokio::test]
async fn late_prompt_response_after_tool_result_survives_quiet_window() {
    init_env();
    let events = run_and_collect(
        AcpHarness::grok(),
        "scenario:quiet-thinking",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert_eq!(
        done_reasons(&events),
        vec![Some(DoneReason::PromptResponse)]
    );
    let working = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::TextDelta { text } if text == "working"))
        .expect("initial response text should be streamed");
    let finished = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::TextDelta { text } if text == "finished"))
        .expect("late response text should be streamed in the same turn");
    let done = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(
        working < finished && finished < done,
        "the quiet interval must not split the turn: {events:?}"
    );
    let quiet_interval = events[finished].0.duration_since(events[working].0);
    assert!(
        quiet_interval > Duration::from_millis(QUIET_MS),
        "fixture's quiet interval {quiet_interval:?} must exceed {QUIET_MS}ms"
    );
}

/// The guard: an OPEN tool call makes silence legitimate. The fixture is
/// quiet for ~3x the settle window mid-tool-call, then finishes normally —
/// exactly one Done, arriving from the real response, never a premature
/// synthesized one.
#[tokio::test]
async fn open_tool_call_holds_the_quiet_settle_off() {
    init_env();
    let events = run_and_collect(
        AcpHarness::grok(),
        "scenario:quiet-tool-guard",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    // The "finished" text (streamed after the quiet stretch) must precede
    // the single Done — a premature settle would have flipped that order.
    let finished = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::TextDelta { text } if text == "finished"))
        .expect("post-quiet text must fold into the SAME turn: {events:?}");
    let done = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(
        finished < done,
        "the turn must survive the quiet stretch intact: {events:?}"
    );
}
