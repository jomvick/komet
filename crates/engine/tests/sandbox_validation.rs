//! Sandbox-options validation gate: a run request whose explicit
//! `sandbox_options` fail `validate_run_request` must fail FAST — before any
//! harness spawn — with the command rejected and an errored turn, and
//! `auto_approve` must never override the explicit options table.

use std::sync::Arc;
use std::time::Duration;

use komet_doc::{MessageRole, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus};
use komet_engine::{EngineCore, HarnessRegistry};
use komet_harness::mock::MockHarness;
use komet_proto::{
    ApprovalPolicy, CodexSandbox, DoneStatus, HarnessId, RunRequest, SandboxLevel, SandboxMode,
    SandboxOptions,
};

const CHAT: &str = "chat-validation";
const VIEWER: &str = "viewer-device";

fn base_request() -> RunRequest {
    RunRequest {
        prompt: "do the thing".into(),
        harness: Some(HarnessId::Mock),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: false,
        attachments: vec![],
        worktree: None,
        resume: None,
    }
}

fn assemble(dir: &std::path::Path) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: vec![
            komet_proto::AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            },
            komet_proto::AgentEvent::TextDelta { text: "ok".into() },
            komet_proto::AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
                reason: None,
            },
        ],
    }));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

fn queue_run(core: &EngineCore, id: &str, request: RunRequest) {
    let handle = core.doc_host.open(CHAT).expect("open chat");
    let now = chrono::Utc::now().timestamp_millis();
    handle
        .doc()
        .queue_command(&SessionCommandEntry {
            id: id.into(),
            payload: SessionCommandPayload::Run {
                request,
                message_id: format!("msg-{id}"),
            },
            issued_by: VIEWER.into(),
            issued_at: now,
            based_on: None,
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
        .find(|c| c.id == id)
        .map(|c| (c.status, c.resolution))
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
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn entries(core: &EngineCore) -> Vec<komet_doc::SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read entries")
}

#[tokio::test]
async fn validation_rejects_writable_root_outside_cwd_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path());
    let mut req = base_request();
    req.sandbox_options = Some(SandboxOptions {
        codex: Some(CodexSandbox {
            writable_roots: vec!["/etc".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    queue_run(&core, "cmd-bad-root", req);

    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-bad-root"),
                Some((SessionCommandStatus::Rejected, _))
            )
        },
        "command to be rejected",
    )
    .await;

    let (status, resolution) = command_status(&core, "cmd-bad-root").unwrap();
    assert_eq!(status, SessionCommandStatus::Rejected);
    let resolution = resolution.expect("resolution carries the reason");
    assert!(
        resolution.contains("writable root") && resolution.contains("/etc"),
        "unexpected resolution: {resolution}"
    );
    // Fail-fast: nothing was spawned, so no user or assistant entry exists.
    assert!(entries(&core).is_empty(), "no entries expected");
}

#[tokio::test]
async fn validation_sandbox_options_win_over_sandbox_level_and_run() {
    // sandbox=DangerFullAccess on the level + explicit options in
    // workspace-write mode: the OPTIONS win, so an outside-cwd writable root
    // is still rejected even though the coarse level says danger.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path());
    let mut req = base_request();
    req.sandbox = SandboxLevel::DangerFullAccess;
    req.sandbox_options = Some(SandboxOptions {
        codex: Some(CodexSandbox {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            writable_roots: vec!["/etc".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    queue_run(&core, "cmd-level-vs-options", req);

    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-level-vs-options"),
                Some((SessionCommandStatus::Rejected, _))
            )
        },
        "command to be rejected despite danger level",
    )
    .await;
}

#[tokio::test]
async fn validation_yolo_does_not_override_explicit_options() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path());
    let mut req = base_request();
    req.auto_approve = true;
    req.sandbox_options = Some(SandboxOptions {
        codex: Some(CodexSandbox {
            approval_policy: Some(ApprovalPolicy::Never),
            network_access: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    queue_run(&core, "cmd-yolo", req);

    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-yolo"),
                Some((SessionCommandStatus::Rejected, _))
            )
        },
        "yolo run to be rejected",
    )
    .await;
    let (_, resolution) = command_status(&core, "cmd-yolo").unwrap();
    let resolution = resolution.expect("resolution present");
    assert!(
        resolution.contains("networkAccess"),
        "expected network-access rejection, got: {resolution}"
    );

    // And a well-formed granular options table WITH auto_approve still runs:
    // yolo never blocks valid explicit options either.
    let dir2 = tempfile::tempdir().unwrap();
    let core2 = assemble(dir2.path());
    let mut ok = base_request();
    ok.auto_approve = true;
    ok.sandbox_options = Some(SandboxOptions {
        codex: Some(CodexSandbox {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            approval_policy: Some(ApprovalPolicy::Granular {
                ask: vec!["rm -rf *".into()],
                auto_approve: vec!["ls".into()],
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    queue_run(&core2, "cmd-yolo-ok", ok);
    wait_for(
        || {
            entries(&core2)
                .iter()
                .any(|e| e.role == MessageRole::Assistant)
        },
        "valid yolo run to complete",
    )
    .await;
}

#[tokio::test]
async fn other_provider_with_options_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path());
    for provider in [
        HarnessId::Cursor,
        HarnessId::Grok,
        HarnessId::Hermes,
        HarnessId::Pi,
        HarnessId::Antigravity,
    ] {
        let mut req = base_request();
        req.harness = Some(provider);
        req.sandbox_options = Some(SandboxOptions {
            codex: Some(CodexSandbox {
                sandbox_mode: Some(SandboxMode::WorkspaceWrite),
                ..Default::default()
            }),
            ..Default::default()
        });
        let cmd_id = format!("cmd-reject-{provider:?}");
        queue_run(&core, &cmd_id, req);
        wait_for(
            || {
                matches!(
                    command_status(&core, &cmd_id),
                    Some((SessionCommandStatus::Rejected, _))
                )
            },
            "non-sandbox provider with options to be rejected",
        )
        .await;
        let (status, resolution) = command_status(&core, &cmd_id).unwrap();
        assert_eq!(status, SessionCommandStatus::Rejected);
        assert!(
            resolution.unwrap().contains("rejected"),
            "provider {provider:?} should be rejected"
        );
    }
    // Empty options must NOT be rejected.
    let dir2 = tempfile::tempdir().unwrap();
    let core2 = assemble(dir2.path());
    let mut empty = base_request();
    empty.harness = Some(HarnessId::Cursor);
    empty.sandbox_options = Some(SandboxOptions::default());
    queue_run(&core2, "cmd-empty-ok", empty);
    // Empty options should pass validation and spawn (assistant entry appears)
    wait_for(
        || entries(&core2).iter().any(|e| e.role == MessageRole::Assistant),
        "empty options should not be rejected",
    )
    .await;
}
