//! AntigravityHarness integration tests against the fake CLI script in
//! `tests/fixtures/fake-agy.sh`.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{AntigravityHarness, CancellationToken, Harness, RunControls, SteerMessage};
use komet_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, ToolCall};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-agy.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> AntigravityHarness {
    AntigravityHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Antigravity),
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: env!("CARGO_MANIFEST_DIR").into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: false,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        request_permission: RunControls::noop_permission(),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn collect_events(
    harness: &AntigravityHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let mut stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("test completes within timeout")
}

#[tokio::test]
async fn happy_path_streams_deltas_and_completes() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:happy"), ctrls).await;

    assert!(events.iter().any(|e| matches!(e, AgentEvent::SessionStarted { .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("Hello from Antigravity"))));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Usage { input_tokens: 100, output_tokens: 20, .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { status: DoneStatus::Completed, .. })));
}

#[tokio::test]
async fn flags_include_dangerously_skip_permissions_and_add_dir() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let req = request("scenario:verify_flags");
    let events = collect_events(&h, req, ctrls).await;

    let done = events.iter().find_map(|e| match e {
        AgentEvent::Done { status, error, .. } => Some((status, error)),
        _ => None,
    }).expect("must have Done event");

    assert_eq!(done.0, &DoneStatus::Completed, "Expected Completed, got error: {:?}", done.1);
}

#[tokio::test]
async fn tool_lifecycle_deduplicates_and_emits_result() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:tool_lifecycle"), ctrls).await;

    // ToolCall must only be emitted once
    let tool_calls: Vec<_> = events.iter().filter_map(|e| match e {
        AgentEvent::ToolCall { id, call } => Some((id, call)),
        _ => None,
    }).collect();
    assert_eq!(tool_calls.len(), 1, "ToolCall must be deduplicated");
    assert_eq!(tool_calls[0].0, "agy-step-1");
    match tool_calls[0].1 {
        ToolCall::Glob { pattern } => assert_eq!(pattern, "/test/project"),
        _ => panic!("Expected Glob tool call for list_dir"),
    }

    // ToolResult must be emitted with the output
    let tool_results: Vec<_> = events.iter().filter_map(|e| match e {
        AgentEvent::ToolResult { id, is_error, output, .. } => Some((id, is_error, output)),
        _ => None,
    }).collect();
    assert_eq!(tool_results.len(), 1, "ToolResult must be emitted");
    assert_eq!(tool_results[0].0, "agy-step-1");
    assert!(!tool_results[0].1);
    assert_eq!(tool_results[0].2.as_deref(), Some("Cargo.toml\nsrc/"));
}

#[tokio::test]
async fn tool_error_emits_error_result_and_captures_diagnostic() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:tool_error"), ctrls).await;

    // ToolResult must be emitted even on error state
    let tool_results: Vec<_> = events.iter().filter_map(|e| match e {
        AgentEvent::ToolResult { id, is_error, output, .. } => Some((id, is_error, output)),
        _ => None,
    }).collect();
    assert_eq!(tool_results.len(), 1, "ToolResult must be emitted on error");
    assert_eq!(tool_results[0].0, "agy-step-1");
    assert!(tool_results[0].1, "ToolResult must mark is_error = true");
    assert!(tool_results[0].2.as_ref().unwrap().contains("bad_cmd: not found"));

    // Done event should reflect error/cancellation with captured diagnostic
    let done = events.iter().find_map(|e| match e {
        AgentEvent::Done { status, error, .. } => Some((status, error)),
        _ => None,
    }).expect("Done event present");
    assert_eq!(done.0, &DoneStatus::Errored);
    assert!(done.1.as_ref().unwrap().contains("jetski: execution failed") || done.1.as_ref().unwrap().contains("CANCELED"));
}

#[tokio::test]
async fn process_crash_reports_stderr() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:crash"), ctrls).await;

    let done = events.iter().find_map(|e| match e {
        AgentEvent::Done { status, error, .. } => Some((status, error)),
        _ => None,
    }).expect("Done event present");
    assert_eq!(done.0, &DoneStatus::Errored);
    let err = done.1.as_ref().expect("error message present");
    assert!(err.contains("Fatal runtime error in agy"), "Expected stderr tail in error, got: {err}");
}

#[tokio::test]
async fn interrupt_cancels_and_terminates() {
    let h = harness();
    let (ctrls, _steer, tok) = controls();
    
    let mut stream = h.run(request("scenario:hang"), ctrls).await.expect("run starts");
    tokio::time::sleep(Duration::from_millis(50)).await;
    tok.cancel();

    let done = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = stream.next().await {
            if let Ok(AgentEvent::Done { status, reason, .. }) = ev {
                return Some((status, reason));
            }
        }
        None
    }).await.expect("timeout").expect("done event");

    assert_eq!(done.0, DoneStatus::Interrupted);
    assert_eq!(done.1, Some(komet_proto::DoneReason::UserRequested));
}

#[tokio::test]
async fn dynamic_model_discovery() {
    let h = harness();
    let models = h.models().await.expect("models query succeeds");
    assert!(models.iter().any(|m| m.id == "gemini-3.8-flash-high"));
    assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
}
