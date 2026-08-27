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

fn opencode_fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-opencode-acp.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn opencode_harness() -> AcpHarness {
    AcpHarness::opencode().with_executable(opencode_fixture_path())
}

fn request_opencode(prompt: &str, model: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: model.map(str::to_owned),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
        worktree: None,
    }
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
        resume: None,
        worktree: None,
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
        request_permission: RunControls::noop_permission(),
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
async fn question_shaped_requests_bridge_to_the_input_panel() {
    // The controls' bridge answers every question with its FIRST option
    // label — build controls that answer "Use tokio" specifically.
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Use tokio".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        request_permission: RunControls::noop_permission(),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let _keep = (steer_tx, token);
    let events = run_to_end(&harness(), request("scenario:question"), controls).await;
    // The fixture answers refusal unless the harness relayed the choice
    // (optionId opt-tokio) instead of auto-accepting.
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "answered".into()
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

/// The steering response racing the turn's own end: the injection landed in
/// the dying turn, and the prompt response reached the wire first. The
/// boundary must still be emitted BEFORE the Done — a Steered after Done
/// re-armed the consumer (parked session → Working) with no next turn and no
/// Done ever coming (the stranded-Working / eternal-timer bug).
#[tokio::test]
async fn steer_racing_the_turn_end_never_emits_steered_after_done() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-race"), controls)
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

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("steer landed in the turn: a Steered boundary must exist");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("checked above");
    assert!(
        steered < done,
        "Steered after Done strands the session: {events:?}"
    );
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

/// Real-adapter smoke: spawns the actual `claude-agent-acp` (via npx when not
/// Discovery against the real installed adapters: base model rows only
/// (never one per reasoning effort), with wire-derived trait options. Free
/// (initialize + session/new, no prompt), but needs the CLIs installed and
/// authenticated. Run explicitly:
/// `cargo test -p komet-harness --test acp -- --ignored real_discovery`
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

#[tokio::test]
async fn models_are_discovered_from_the_acp_session() {
    // ACP is the source of truth: the fixture advertises a model config
    // option, so the picker list comes from the wire, not the static catalog.
    let harness = AcpHarness::hermes().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["grok-4-fast", "grok-4.5"], "{models:?}");
    // Unmatched ids inherit the probe session's thought_level ladder.
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            komet_proto::ReasoningLevel::Low,
            komet_proto::ReasoningLevel::Medium,
            komet_proto::ReasoningLevel::High,
        ],
        "{models:?}"
    );
    assert_eq!(models[0].description.as_deref(), Some("Fast tier"));
    // Cached: a second call returns the same list without respawning.
    let again = harness.models().await.expect("cached");
    assert_eq!(again, models);
}

#[tokio::test]
async fn models_enrich_from_the_static_catalog_on_id_match() {
    // grok's static catalog knows "grok-4.5" — the discovered entry keeps the
    // wire label but inherits the curated description and ladder.
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let grok45 = models
        .iter()
        .find(|m| m.id == "grok-4.5")
        .expect("grok-4.5");
    assert_eq!(
        grok45.description.as_deref(),
        Some("xAI's coding model — 500k context"),
        "{grok45:?}"
    );
}

#[tokio::test]
async fn models_fall_back_to_the_static_catalog_when_the_probe_fails() {
    let harness = AcpHarness::pi().with_executable("/nonexistent/never-a-pi-acp");
    let models = harness.models().await.expect("static fallback");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["default"], "{models:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn hung_handshake_errors_instead_of_spinning_forever() {
    // An agent that consumes stdin and never answers initialize — the
    // "thinking for minutes, then nothing" startup class (issue #93). The
    // run must end with a Done that names the timeout, not hang.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hung-agent.sh");
    // sleep inherits the stdio pipes and holds them open without ever
    // answering — a true wedge, not a crash.
    std::fs::write(&script, "#!/bin/sh\nexec sleep 1000\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let harness = AcpHarness::grok()
        .with_executable(&script)
        .with_handshake_timeout(Duration::from_millis(300));
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness, request("hi"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    let (status, error) = &dones[0];
    assert_eq!(*status, DoneStatus::Errored);
    let error = error.as_deref().unwrap_or_default();
    assert!(
        error.contains("did not complete the ACP handshake"),
        "{error}"
    );
}

#[test]
fn hermes_and_pi_descriptor_surfaces_match_registry_expectations() {
    let hermes = AcpHarness::hermes();
    assert_eq!(hermes.id(), HarnessId::Hermes);
    assert_eq!(hermes.display_name(), "Hermes");
    assert!(hermes.supports_steering());
    assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
    assert!(hermes.reasoning_levels().is_empty());

    let pi = AcpHarness::pi();
    assert_eq!(pi.id(), HarnessId::Pi);
    assert_eq!(pi.display_name(), "Pi");
    assert!(pi.supports_steering());
    assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        pi.reasoning_levels(),
        &[
            komet_proto::ReasoningLevel::Minimal,
            komet_proto::ReasoningLevel::Low,
            komet_proto::ReasoningLevel::Medium,
            komet_proto::ReasoningLevel::High,
            komet_proto::ReasoningLevel::XHigh,
            komet_proto::ReasoningLevel::Max,
        ]
    );
}

/// The 2026-08-12 stuck-Working wedge, end to end: a prompt whose turn was
/// consumed by CLI-side self-continuation never gets its response. A steer's
/// `noRunningTurn` steering outcome is the protocol evidence the pending
/// prompt can never settle; after the grace the harness closes the dead turn
/// (Done — never a stranded Working) and promotes the steer to a fresh
/// prompt, which settles normally.
#[tokio::test]
async fn starved_prompt_recovers_via_no_running_turn_evidence() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:starve"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                steer
                    .send(SteerMessage {
                        prompt: "what about now".into(),
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

    // Two settled turns: the synthesized close of the starved prompt, then
    // the promoted steer's real turn.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "promoted".into()
    }));
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("the queued steer must be promoted through a Steered boundary");
    let first_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("dones asserted above");
    assert!(
        first_done < steered,
        "the dead turn settles before the promoted boundary: {events:?}"
    );
}

#[tokio::test]
#[ignore = "needs the claude CLI authenticated + network; costs a few small prompts"]
async fn steer_into_self_continuation_cancels_before_prompting() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:busy-steer"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            // The self-continued tool call is the busy signal: steer now.
            if matches!(&ev, AgentEvent::ToolCall { id, .. } if id == "sc-1")
                && let Some(tx) = steer.take()
            {
                tx.send(SteerMessage {
                    prompt: "what about now".into(),
                    message_id: None,
                })
                .await
                .expect("steer sent");
                // Sender dropped here; the mailbox closes so the run can end.
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // Two clean turns: the first prompt's, then the promoted steer's —
    // and the fixture exits with `refusal` if a prompt ever arrives
    // without the preceding session/cancel.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fresh answer".into()
    }));
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("promoted steer must carry a boundary");
    let first_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("dones asserted above");
    assert!(first_done < steered, "{events:?}");
}

/// Claude's native busy-steer path: a steer into a self-continued turn goes
/// out as a PLAIN prompt (the fixture hard-fails on any session/cancel —
/// cancelling would kill the agent's in-flight work). The CLI folds the
/// message into the running turn natively; the adapter drops the prompt's
/// reply; the cost-frame settle closes the turn ~1s after the merged turn
/// really ends — well before the fixture's held-open stream EOF.
#[tokio::test]
async fn injection_cost_frame_never_settles_a_steered_turn() {
    let (controls, steer, _token) = controls();
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let stream = harness
        .run(request("scenario:steer-cost-noise"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first")
                && let Some(tx) = steer.take()
            {
                tx.send(SteerMessage {
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

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "a premature cost-frame settle would double-Done: {events:?}"
    );
    // The post-injection text arrives BEFORE the single Done — a false
    // settle would flip that order.
    let tail = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "steered tail"))
        .expect("steered tail must fold into the live turn: {events:?}");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(tail < done, "{events:?}");
}

#[tokio::test]
async fn opencode_probe_failure_surfaces_as_an_error() {
    // opencode ships no static catalog — the wire is the only source. A
    // failed probe must ERROR (the picker shows a Retry row) rather than
    // silently read as an empty list (which rendered an eternal loading
    // skeleton — user report: "opencode doesn't show models, the picker
    // seems frozen"). Catalog-backed harnesses still fall back to their
    // static list (covered by the catalog harnesses' own tests).
    let harness = AcpHarness::opencode().with_executable("/nonexistent/never-an-opencode-acp");
    let err = harness
        .models()
        .await
        .expect_err("wire-only probe failure errors");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

#[tokio::test]
async fn catalog_harness_failed_probe_still_falls_back_to_static_models() {
    // The error-propagation rule is wire-only: a catalog-backed harness
    // (cursor here) keeps the static fallback on a failed probe.
    let harness = AcpHarness::grok().with_executable("/nonexistent/never-a-cursor-agent");
    let models = harness.models().await.expect("static fallback");
    assert!(!models.is_empty(), "{models:?}");
}

#[tokio::test]
async fn opencode_launches_with_acp_args_and_runs_the_happy_path() {
    // The fixture refuses any launch without the `acp` argument, proving the
    // spec's args land on the wire. Model ids are opencode-flavored, so the
    // run carries no model set and the chat settles.
    let (controls, _steer, _token) = controls();
    let events = run_to_end(
        &opencode_harness(),
        request_opencode("scenario:happy", None),
        controls,
    )
    .await;
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello from opencode".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn opencode_sets_only_the_requested_model() {
    // The requested model differs from the session's currentValue, so the
    // harness sends one `session/set_config_option` for it; the fixture
    // answers "configured" only when that set (and no thought_level set,
    // which opencode has no option for) arrived.
    let (controls, _steer, _token) = controls();
    let events = run_to_end(
        &opencode_harness(),
        request_opencode("scenario:config", Some("opencode/smol")),
        controls,
    )
    .await;
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "configured".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn opencode_overlay_injected() {
    use komet_proto::{BashPerms, OpenCodePerms, Perm, SandboxOptions};
    let mut req = request_opencode("scenario:overlay", None);
    req.sandbox_options = Some(SandboxOptions {
        opencode: Some(OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".to_owned(), Perm::Ask)],
            },
            unscoped_actions: Default::default(),
        }),
        ..Default::default()
    });
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&opencode_harness(), req, controls).await;
    assert!(
        events.contains(&AgentEvent::TextDelta { text: "overlay ok".into() }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn opencode_steering_delivers_at_turn_boundary() {
    // No `_session/steering` extension on opencode's wire: a steer must never
    // ride `_session/steering` (the fixture would refuse it). It settles the
    // current turn and is promoted to a fresh session/prompt — the fixture
    // answers "boundary" for that plain prompt.
    let (controls, steer, _token) = controls();
    let harness = opencode_harness();
    let stream = harness
        .run(
            request_opencode("scenario:steer-tb", Some("opencode/big-pickle")),
            controls,
        )
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

    // Two settled turns: the initial prompt, then the promoted steer.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "boundary".into()
    }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
}
