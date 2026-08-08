//! AcpHarness integration tests against the fake ACP agent in
//! `tests/fixtures/fake-acp.sh` (no real `grok` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{
    AcpHarness, CancellationToken, Harness, HarnessError, RunControls, SteerMessage,
};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SteeringMode, TodoItem, ToolCall,
    UserInputAnswer,
};

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

fn harness() -> AcpHarness {
    AcpHarness::grok().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("grok-4.5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn run_to_end(
    harness: &AcpHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

fn dones(events: &[AgentEvent]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn happy_path_maps_chunks_tools_diffs_plans_and_commands() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;

    // SessionStarted from session/new's id.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { harness, session_id, cwd, .. }
                if *harness == HarnessId::Grok && session_id == "s-1" && cwd == "/tmp"
        )),
        "{events:?}"
    );

    // Initialize-advertised commands surface before the turn.
    let commands: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AvailableCommands { commands } => Some(commands.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(commands.len(), 2, "{events:?}");
    assert_eq!(commands[0][0].name, "compact");
    assert_eq!(commands[0][1].input_hint.as_deref(), Some("the goal"));
    // Mid-run advertisement replaces the list.
    assert_eq!(commands[1][0].name, "deep-research");

    // Chunks; the wrong-session and non-text chunks never surface.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking".into()
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("WRONG"))),
        "{events:?}"
    );

    // Execute tool: pending opens the call, the completed update resolves it
    // with capped multi-line output (newlines preserved verbatim).
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t1".into(),
        call: ToolCall::Exec {
            command: "cargo test -p komet-harness".into()
        },
    }));
    let exec_output = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error: false,
                output: Some(output),
                ..
            } if id == "t1" => Some(output.clone()),
            _ => None,
        })
        .expect("exec output present");
    assert!(exec_output.starts_with("   Compiling komet-harness"));
    assert_eq!(exec_output.lines().count(), 6, "{exec_output:?}");

    // Edit tool: single-shot completed call carries the inline diff.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t2".into(),
        call: ToolCall::EditFile {
            path: "/w/src/resolve.rs".into(),
            old_string: None,
            new_string: None,
        },
    }));
    let diff = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                diff: Some(diff),
                ..
            } if id == "t2" => Some(diff.clone()),
            _ => None,
        })
        .expect("edit diff present");
    assert_eq!(diff.path, "/w/src/resolve.rs");
    assert!(
        diff.old_text
            .as_deref()
            .is_some_and(|t| t.contains(".filter(|p| p.exists())")),
        "{diff:?}"
    );
    assert!(diff.new_text.contains("split_paths"), "{diff:?}");

    // Plan → stable todo chip.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "acp-plan".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "read".into(),
                    done: true
                },
                TodoItem {
                    text: "fix".into(),
                    done: false
                },
            ]
        },
    }));

    // usage_update maps to nothing (context gauge, not per-turn tokens).
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Usage { .. })));

    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn config_options_apply_requested_model_and_effort() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:config");
    req.reasoning = Some(komet_proto::ReasoningLevel::Medium);
    let events = run_to_end(&harness(), req, controls).await;
    // The fixture answers refusal unless BOTH set_config_option calls
    // (model grok-4.5, effort medium) arrived before the prompt.
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "configured".into()
        }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn permission_requests_auto_accept_the_preferred_allow_option() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:permission"), controls).await;
    // The fixture answers refusal unless the harness selected "always".
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "approved".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn steering_extension_injects_mid_turn() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-ext"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first") {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn rejected_steer_queues_and_delivers_at_the_turn_boundary() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-queue"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first")
                && let Some(steer) = &steer
            {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            // Close the mailbox once the boundary turn streams so the
            // persistent session winds down and the stream ends.
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "boundary") {
                steer = None;
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // First turn completes, then the queued steer becomes the boundary turn.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "boundary".into()
    }));
}

#[tokio::test]
async fn interrupt_sends_session_cancel_and_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");
    assert_eq!(dones(&events), vec![(DoneStatus::Interrupted, None)]);
}

#[tokio::test]
async fn wedged_agent_escalates_to_signals_and_still_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness().with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("escalation reaped the child in time");
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    assert_eq!(dones[0].0, DoneStatus::Interrupted);
}

#[tokio::test]
async fn refusal_maps_to_an_errored_done() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:refusal"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1);
    assert_eq!(dones[0].0, DoneStatus::Errored);
    assert!(dones[0].1.as_deref().unwrap_or("").contains("refused"));
}

#[tokio::test]
async fn resume_loads_the_session_and_drops_replayed_history() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:resumed");
    req.resume = Some("s-loaded".into());
    let events = run_to_end(&harness(), req, controls).await;
    // The 600-update replay is drained without surfacing…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("old reply"))),
        "{events:?}"
    );
    // …the loaded session id sticks, and the live turn still streams.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "s-loaded"
    )));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "back again".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn failed_load_falls_back_to_a_fresh_session() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:resumed");
    req.resume = Some("load-fail".into());
    let events = run_to_end(&harness(), req, controls).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "s-fresh"
    )));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn commands_discovery_scans_the_initialize_response() {
    let harness = harness();
    let commands = harness.commands().await.expect("discovery");
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0].name, "compact");
    assert_eq!(commands[1].name, "goal");
    assert_eq!(commands[1].input_hint.as_deref(), Some("the goal"));
    // Cached: a second call must not respawn (same result, instant).
    let again = harness.commands().await.expect("cached");
    assert_eq!(again, commands);
}

#[tokio::test]
async fn missing_binary_surfaces_not_installed_with_install_hint() {
    let harness = AcpHarness::grok().with_executable("/nonexistent/definitely-not-grok");
    let err = harness
        .run(request("x"), controls().0)
        .await
        .err()
        .expect("missing binary must fail");
    assert!(matches!(
        err,
        HarnessError::NotInstalled(_) | HarnessError::Io(_)
    ));
}

#[test]
fn descriptor_surface_matches_registry_expectations() {
    let harness = AcpHarness::grok();
    assert_eq!(harness.id(), HarnessId::Grok);
    assert_eq!(harness.display_name(), "Grok");
    assert!(harness.supports_steering());
    assert_eq!(harness.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        harness.reasoning_levels(),
        &[
            komet_proto::ReasoningLevel::Low,
            komet_proto::ReasoningLevel::Medium,
            komet_proto::ReasoningLevel::High,
        ]
    );
}
