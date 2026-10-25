#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{StreamExt, stream::BoxStream};
use komet_harness::{
    CancellationToken, Harness, HarnessError, MuseHarness, RunControls, SteerMessage,
};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, McpInjection, McpServerConfig, McpTransport,
    PermissionChoice, PermissionKind, ReasoningLevel, ResolvedMcpServer, RunRequest, SandboxLevel,
    SteeringMode,
};
use tokio::sync::{mpsc, oneshot};

type Events = BoxStream<'static, Result<AgentEvent, HarnessError>>;

struct TestRun {
    stream: Events,
    steering: mpsc::Sender<SteerMessage>,
    interrupt: CancellationToken,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn test_harness(dir: &Path, scenario: &str) -> MuseHarness {
    let wrapper = dir.join("muse");
    let fixture = fixture_dir().join("fake-muse.py");
    let log = dir.join("frames.ndjson");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nMUSE_TEST_SCENARIO={scenario} MUSE_TEST_LOG={} exec python3 {} \"$@\"\n",
            log.display(),
            fixture.display(),
        ),
    )
    .expect("write Muse test wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("make Muse test wrapper executable");
    MuseHarness::new()
        .with_executable(wrapper)
        .with_graces(Duration::from_millis(500), Duration::from_millis(500))
}

fn request(dir: &Path) -> RunRequest {
    let cwd = dir.join("workspace");
    std::fs::create_dir_all(&cwd).expect("create test workspace");
    RunRequest {
        prompt: "hello".into(),
        harness: Some(HarnessId::Muse),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: false,
        resume: None,
        attachments: vec![],
        worktree: None,
        permission_timeout_ms: None,
        mcp: None,
        mcp_external: vec![],
    }
}

fn controls(
    request_permission: impl Fn(
        PermissionKind,
        String,
        Vec<PermissionChoice>,
    ) -> oneshot::Receiver<PermissionChoice>
    + Send
    + Sync
    + 'static,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steering_tx, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    (
        RunControls {
            request_input: Box::new(|_| {
                let (_tx, rx) = oneshot::channel();
                rx
            }),
            request_permission: Box::new(request_permission),
            steering,
            interrupt: interrupt.clone(),
        },
        steering_tx,
        interrupt,
    )
}

fn deny_permission(
    kind: PermissionKind,
    summary: String,
    choices: Vec<PermissionChoice>,
) -> oneshot::Receiver<PermissionChoice> {
    RunControls::noop_permission()(kind, summary, choices)
}

async fn start(
    harness: &MuseHarness,
    request: RunRequest,
    permission: impl Fn(
        PermissionKind,
        String,
        Vec<PermissionChoice>,
    ) -> oneshot::Receiver<PermissionChoice>
    + Send
    + Sync
    + 'static,
) -> TestRun {
    let (run_controls, steering, interrupt) = controls(permission);
    TestRun {
        stream: harness
            .run(request, run_controls)
            .await
            .expect("start Muse"),
        steering,
        interrupt,
    }
}

async fn until_done(stream: &mut Events) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("Muse event timed out")
            .expect("Muse stream ended before Done")
            .expect("Muse run failed");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn finish_status(events: &[AgentEvent]) -> DoneStatus {
    match events.last() {
        Some(AgentEvent::Done { status, .. }) => *status,
        event => panic!("expected Done event, got {event:?}"),
    }
}

fn frames(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("frames.ndjson"))
        .expect("Muse should log protocol frames")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON frame"))
        .collect()
}

#[test]
fn muse_descriptor_declares_its_native_surface() {
    let muse = MuseHarness::new();
    assert_eq!(muse.id(), HarnessId::Muse);
    assert_eq!(muse.display_name(), "Muse Code");
    assert!(muse.supports_steering());
    assert_eq!(muse.steering_mode(), SteeringMode::StepBoundary);
    assert!(muse.reasoning_levels().is_empty());
}

#[tokio::test]
async fn model_discovery_reads_the_native_catalog() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "complete");

    let models = harness.models().await.expect("discover models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "muse-spark-test");
    assert_eq!(models[0].label, "Muse Spark Test");
    assert_eq!(
        models[0].reasoning_levels,
        [ReasoningLevel::Low, ReasoningLevel::High]
    );
}

#[tokio::test]
async fn permissions_use_the_engine_bridge_and_restricted_auto_approval_keeps_sandbox() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "permission");
    let request = request(dir.path());
    let (permission_tx, mut permission_rx) = mpsc::unbounded_channel();
    let mut run = start(&harness, request, move |kind, summary, _choices| {
        let _ = permission_tx.send((kind, summary));
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(PermissionChoice::Allow);
        rx
    })
    .await;

    let events = until_done(&mut run.stream).await;
    assert_eq!(finish_status(&events), DoneStatus::Completed);
    let (kind, summary) = permission_rx.recv().await.expect("permission callback");
    assert_eq!(
        kind,
        PermissionKind::Command {
            cmdline: "touch marker".into()
        }
    );
    assert!(summary.contains("touch marker"));

    let launch_args: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("frames.ndjson.args")).expect("read launch args"),
    )
    .expect("parse launch args");
    assert!(!launch_args.iter().any(|arg| arg == "--disable-sandbox"));
    let frames = frames(dir.path());
    assert!(frames.iter().any(|frame| {
        frame["method"] == "approval/decide" && frame["params"]["choiceId"] == "allow-once"
    }));
    drop(run.steering);
}

#[tokio::test]
async fn auto_approval_does_not_disable_a_restricted_sandbox() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "complete");
    let mut request = request(dir.path());
    request.auto_approve = true;
    let mut run = start(&harness, request, deny_permission).await;
    let events = until_done(&mut run.stream).await;
    assert_eq!(finish_status(&events), DoneStatus::Completed);
    let launch_args: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("frames.ndjson.args")).expect("read launch args"),
    )
    .expect("parse launch args");
    assert!(!launch_args.iter().any(|arg| arg == "--disable-sandbox"));
    drop(run.steering);
}

#[tokio::test]
async fn resumed_sessions_keep_their_native_session_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "complete");
    let mut request = request(dir.path());
    request.resume = Some("previous-session".into());
    let mut run = start(&harness, request, deny_permission).await;

    let events = until_done(&mut run.stream).await;
    assert_eq!(finish_status(&events), DoneStatus::Completed);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted {
            harness: HarnessId::Muse,
            session_id,
            ..
        } if session_id == "previous-session"
    )));
    assert!(frames(dir.path()).iter().any(|frame| {
        frame["method"] == "session/resume" && frame["params"]["sessionId"] == "previous-session"
    }));
    drop(run.steering);
}

#[tokio::test]
async fn steering_and_cancellation_use_msp_turn_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "steer");
    let mut run = start(&harness, request(dir.path()), deny_permission).await;

    let first = tokio::time::timeout(Duration::from_secs(5), run.stream.next())
        .await
        .expect("session startup timed out")
        .expect("stream ended")
        .expect("startup event");
    assert!(matches!(first, AgentEvent::SessionStarted { .. }));
    run.steering
        .send(SteerMessage {
            prompt: "continue".into(),
            message_id: None,
        })
        .await
        .expect("send steer");
    let events = until_done(&mut run.stream).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Steered { .. }))
    );
    assert!(
        frames(dir.path())
            .iter()
            .any(|frame| frame["method"] == "turn/start" && frame["params"]["ifBusy"] == "steer")
    );
    drop(run.steering);

    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "cancel");
    let mut run = start(&harness, request(dir.path()), deny_permission).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), run.stream.next())
        .await
        .expect("session startup timed out")
        .expect("stream ended")
        .expect("startup event");
    run.interrupt.cancel();
    let events = until_done(&mut run.stream).await;
    assert_eq!(finish_status(&events), DoneStatus::Interrupted);
    assert!(
        frames(dir.path())
            .iter()
            .any(|frame| frame["method"] == "turn/interrupt")
    );
    drop(run.steering);
}

#[tokio::test]
async fn unsupported_mcp_is_reported_in_the_run_stream() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "complete");
    let mut request = request(dir.path());
    request.mcp = Some(McpInjection {
        server_name: "komet".into(),
        url: "http://127.0.0.1:1/mcp".into(),
        auth_token: "test-token".into(),
    });
    let mut run = start(&harness, request, deny_permission).await;

    let events = until_done(&mut run.stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { message, .. } if message.contains("does not currently support Komet")
    )));
    drop(run.steering);
}

#[tokio::test]
async fn unsupported_external_mcp_is_reported_in_the_run_stream() {
    let dir = tempfile::tempdir().expect("temp dir");
    let harness = test_harness(dir.path(), "complete");
    let mut request = request(dir.path());
    request.mcp_external.push(ResolvedMcpServer {
        config: McpServerConfig {
            id: "external".into(),
            name: "External MCP".into(),
            transport: McpTransport::Http,
            url: Some("http://127.0.0.1:1/mcp".into()),
            ..Default::default()
        },
        resolved_headers: Default::default(),
        resolved_env: Default::default(),
    });
    let mut run = start(&harness, request, deny_permission).await;

    let events = until_done(&mut run.stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { message, .. } if message.contains("does not currently support Komet")
    )));
    drop(run.steering);
}
