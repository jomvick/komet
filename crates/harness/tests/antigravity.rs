//! AntigravityHarness integration tests against the fake CLI script in
//! `tests/fixtures/fake-agy.sh`.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{
    AntigravityHarness, CancellationToken, Harness, HarnessError, RunControls, SteerMessage,
};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, ToolCall,
};

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
        mcp: None,
        mcp_external: Vec::new(),
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_, _| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        request_permission: RunControls::noop_permission(),
        steering: steer_rx,
        interrupt: token.clone(),
        env: Vec::new(),
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

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { .. }))
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::TextDelta { text } if text.contains("Hello from Antigravity"))
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 20,
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

/// `agy -p` is a one-shot NDJSON stream: an in-run `ask_question` can never be
/// answered back into it. The harness must not put an unanswerable panel on
/// screen (the old bridge did, then dropped the awaiting receiver); it surfaces
/// a visible notice and keeps the capability table honest.
#[tokio::test]
async fn ask_question_is_a_visible_notice_not_an_unanswerable_panel() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let h = harness();
    let asked = Arc::new(AtomicUsize::new(0));
    let asked_in = asked.clone();
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_, _| {
            asked_in.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        request_permission: RunControls::noop_permission(),
        steering: steer_rx,
        interrupt: token.clone(),
        env: Vec::new(),
    };
    let events = collect_events(&h, request("scenario:ask_question"), controls).await;

    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "no answer channel exists, so no question panel may be opened"
    );
    let notice = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("the unanswerable question must surface as a notice");
    assert!(notice.contains("Which database should I use?"), "{notice}");
    assert!(notice.contains("one-shot"), "{notice}");
    // The tool itself is still visible, and the turn completes.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
    let _ = steer_tx;
}

#[tokio::test]
async fn flags_include_dangerously_skip_permissions_and_add_dir() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let req = request("scenario:verify_flags");
    let events = collect_events(&h, req, ctrls).await;

    let done = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((status, error)),
            _ => None,
        })
        .expect("must have Done event");

    assert_eq!(
        done.0,
        &DoneStatus::Completed,
        "Expected Completed, got error: {:?}",
        done.1
    );
}

#[tokio::test]
async fn workspace_write_enables_agy_sandbox() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let mut req = request("scenario:verify_sandbox");
    req.sandbox = SandboxLevel::WorkspaceWrite;
    let events = collect_events(&h, req, ctrls).await;
    let ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    });
    assert!(ok, "workspace-write must pass --sandbox: {events:?}");
}

#[tokio::test]
async fn read_only_enables_agy_plan_mode() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let mut req = request("scenario:verify_readonly");
    req.sandbox = SandboxLevel::ReadOnly;
    let events = collect_events(&h, req, ctrls).await;
    let ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    });
    assert!(ok, "read-only must pass --sandbox --mode plan: {events:?}");
}

#[tokio::test]
async fn tool_lifecycle_deduplicates_and_emits_result() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:tool_lifecycle"), ctrls).await;

    // ToolCall must only be emitted once
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, call } => Some((id, call)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1, "ToolCall must be deduplicated");
    assert_eq!(tool_calls[0].0, "agy-step-1");
    match tool_calls[0].1 {
        ToolCall::Glob { pattern } => assert_eq!(pattern, "/test/project"),
        _ => panic!("Expected Glob tool call for list_dir"),
    }

    // ToolResult must be emitted with the output
    let tool_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error,
                output,
                ..
            } => Some((id, is_error, output)),
            _ => None,
        })
        .collect();
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
    let tool_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error,
                output,
                ..
            } => Some((id, is_error, output)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1, "ToolResult must be emitted on error");
    assert_eq!(tool_results[0].0, "agy-step-1");
    assert!(tool_results[0].1, "ToolResult must mark is_error = true");
    assert!(
        tool_results[0]
            .2
            .as_ref()
            .unwrap()
            .contains("bad_cmd: not found")
    );

    // Done event should reflect error/cancellation with captured diagnostic
    let done = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((status, error)),
            _ => None,
        })
        .expect("Done event present");
    assert_eq!(done.0, &DoneStatus::Errored);
    assert!(
        done.1
            .as_ref()
            .unwrap()
            .contains("jetski: execution failed")
            || done.1.as_ref().unwrap().contains("CANCELED")
    );
}

#[tokio::test]
async fn process_crash_reports_stderr() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let events = collect_events(&h, request("scenario:crash"), ctrls).await;

    let done = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((status, error)),
            _ => None,
        })
        .expect("Done event present");
    assert_eq!(done.0, &DoneStatus::Errored);
    let err = done.1.as_ref().expect("error message present");
    assert!(
        err.contains("Fatal runtime error in agy"),
        "Expected stderr tail in error, got: {err}"
    );
}

#[tokio::test]
async fn interrupt_cancels_and_terminates() {
    let h = harness();
    let (ctrls, _steer, tok) = controls();

    let mut stream = h
        .run(request("scenario:hang"), ctrls)
        .await
        .expect("run starts");
    tokio::time::sleep(Duration::from_millis(50)).await;
    tok.cancel();

    let done = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = stream.next().await {
            if let Ok(AgentEvent::Done { status, reason, .. }) = ev {
                return Some((status, reason));
            }
        }
        None
    })
    .await
    .expect("timeout")
    .expect("done event");

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

#[tokio::test]
async fn antigravity_failed_probe_returns_err() {
    // Live-only: a missing binary surfaces NotInstalled, never a list.
    let missing = AntigravityHarness::new().with_executable("/nonexistent/never-an-agy");
    let err = missing.models().await.expect_err("missing binary errors");
    assert!(
        matches!(err, HarnessError::NotInstalled(_)),
        "readable install error, got: {err:?}"
    );
    // A failing `agy models` (non-zero exit, login hint on stderr) surfaces
    // a Protocol error naming agy — never the static catalog.
    let failing_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-agy-fail-models.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&failing_path, std::fs::Permissions::from_mode(0o755));
    }
    let failing = AntigravityHarness::new().with_executable(failing_path);
    let err = failing.models().await.expect_err("failing probe errors");
    assert!(
        matches!(err, HarnessError::Protocol(_)),
        "readable protocol error, got: {err:?}"
    );
    assert!(
        err.to_string().contains("agy"),
        "error names the CLI, got: {err}"
    );
}

#[tokio::test]
async fn commands_come_from_the_antigravity_catalog() {
    let commands = harness().commands(None).await.expect("catalog");
    assert!(commands.iter().any(|c| c.name == "goal"));
    assert!(commands.iter().any(|c| c.name == "schedule"));
    assert!(commands.iter().any(|c| c.name == "learn"));
}

#[tokio::test]
async fn claude_does_not_forward_unsupported_effort() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let mut req = request("scenario:happy");
    req.model = Some("claude-sonnet-4-6".into());
    req.reasoning = Some(ReasoningLevel::Medium);
    let events = collect_events(&h, req, ctrls).await;
    let ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    });
    assert!(
        ok,
        "claude-sonnet-4-6 must not receive --effort: {events:?}"
    );
}

#[tokio::test]
async fn suffixed_gemini_does_not_forward_conflicting_effort() {
    let h = harness();
    let (ctrls, _steer, _tok) = controls();
    let mut req = request("scenario:happy");
    req.model = Some("gemini-3.7-flash-high".into());
    req.reasoning = Some(ReasoningLevel::Medium);
    let events = collect_events(&h, req, ctrls).await;
    let ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    });
    assert!(
        ok,
        "gemini-3.8-flash-high must not receive --effort medium: {events:?}"
    );
}
