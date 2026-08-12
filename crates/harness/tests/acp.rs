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
        harness: None,
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
async fn claude_and_codex_specs_drive_the_same_wire() {
    // The whole point of the conversion: every spec runs against the same
    // fake ACP agent with no per-agent protocol code. Model ids in the
    // fixture are grok-flavored, so config sets simply skip.
    for (name, h) in [
        (
            "claude",
            AcpHarness::claude().with_executable(fixture_path()),
        ),
        ("codex", AcpHarness::codex().with_executable(fixture_path())),
        (
            "hermes",
            AcpHarness::hermes().with_executable(fixture_path()),
        ),
        ("pi", AcpHarness::pi().with_executable(fixture_path())),
        (
            "cursor",
            AcpHarness::cursor().with_executable(fixture_path()),
        ),
    ] {
        let (controls, _steer, _token) = controls();
        let events = run_to_end(&h, request("scenario:happy"), controls).await;
        assert!(
            events.contains(&AgentEvent::TextDelta {
                text: "Hello".into()
            }),
            "{name}: {events:?}"
        );
        assert_eq!(
            dones(&events),
            vec![(DoneStatus::Completed, None)],
            "{name}"
        );
    }
}

#[tokio::test]
async fn ultrathink_prefixes_the_prompt_for_claude() {
    let (controls, _steer, _token) = controls();
    let h = AcpHarness::claude().with_executable(fixture_path());
    let mut req = request("scenario:echo-prompt");
    req.reasoning = Some(komet_proto::ReasoningLevel::Ultrathink);
    let events = run_to_end(&h, req, controls).await;
    // The fixture echoes the prompt text back; the Ultrathink prefix must be
    // on the wire.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.starts_with("Ultrathink:")
        )),
        "{events:?}"
    );
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
#[tokio::test]
#[ignore = "needs the claude + codex CLIs installed and authenticated"]
async fn real_discovery_yields_base_models_with_traits() {
    let codex = AcpHarness::codex().models().await.expect("codex discovery");
    assert!(!codex.is_empty());
    for m in &codex {
        assert!(
            !m.id.contains('[') || m.id.ends_with("[1m]"),
            "effort-variant leaked as a model row: {}",
            m.id
        );
    }
    let sol = codex.iter().find(|m| m.id == "gpt-5.6-sol").expect("sol");
    assert!(
        sol.options.iter().any(|o| o.id == "fast-mode"),
        "codex fast-mode trait missing: {:?}",
        sol.options
    );
    assert!(!sol.reasoning_levels.is_empty());

    let claude = AcpHarness::claude()
        .models()
        .await
        .expect("claude discovery");
    assert!(!claude.is_empty());
    for m in &claude {
        assert!(
            !m.reasoning_levels.is_empty(),
            "claude ladder missing on {}",
            m.id
        );
        assert!(
            m.reasoning_levels
                .contains(&komet_proto::ReasoningLevel::Ultrathink),
            "ultrathink extra missing on {}",
            m.id
        );
    }
}

/// installed) against the installed, authenticated claude CLI and burns one
/// tiny haiku prompt. Run explicitly:
/// `cargo test -p komet-harness --test acp -- --ignored real_claude`
#[tokio::test]
#[ignore = "needs the claude CLI authenticated + network; costs one tiny prompt"]
async fn real_claude_adapter_end_to_end() {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::claude();
    let mut req = request("Reply with exactly the word ACP-OK and nothing else.");
    req.model = Some("claude-haiku-4-5".into());
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let stream = harness.run(req, controls).await.expect("run starts");
    // The session parks after Done while the steering mailbox lives (the
    // engine reaps by dropping it) — release the sender at Done or the
    // stream never ends.
    let events = tokio::time::timeout(Duration::from_secs(180), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("ACP-OK"),
        "unexpected reply: {text:?}\n{events:?}"
    );
    assert_eq!(dones(&events).len(), 1, "{events:?}");
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
}

/// The Cursor slot against the real `cursor-agent acp` server: discovery
/// (free) plus one tiny prompt. Run explicitly:
/// `cargo test -p komet-harness --test acp -- --ignored real_cursor`
#[tokio::test]
#[ignore = "needs the cursor-agent CLI authenticated + network; costs one tiny prompt"]
async fn real_cursor_adapter_end_to_end() {
    // `session/new` is the source of truth for models — the static catalog is
    // a fallback, so a live account must produce more than it.
    let models = AcpHarness::cursor().models().await.expect("discovery");
    assert!(models.len() > 5, "{models:?}");
    assert!(
        models
            .iter()
            .any(|m| m.id == "composer-2.5" || m.id.starts_with("composer-2.5")),
        "{models:?}"
    );
    // Parameterized picker: base ids, no raw HTML, Mode on every row, Auto
    // exposes Intelligence / Balance / Cost.
    assert!(
        models.iter().all(|m| !m.label.contains('<')),
        "html leaked into labels: {:?}",
        models
            .iter()
            .filter(|m| m.label.contains('<'))
            .map(|m| &m.label)
            .collect::<Vec<_>>()
    );
    assert!(
        models
            .iter()
            .all(|m| m.options.iter().any(|o| o.id == "mode")),
        "Mode trait missing"
    );
    let auto = models
        .iter()
        .find(|m| m.id == "auto-smart")
        .expect("parameterized Auto id");
    let optimize = auto
        .options
        .iter()
        .find(|o| o.id == "optimize_for")
        .expect("Optimize For");
    let tiers: Vec<&str> = optimize.choices.iter().map(|c| c.id.as_str()).collect();
    assert!(tiers.contains(&"intelligence"), "{tiers:?}");
    assert!(tiers.contains(&"balanced"), "{tiers:?}");
    assert!(tiers.contains(&"cost"), "{tiers:?}");

    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::cursor();
    let mut req = request("Reply with exactly the word ACP-OK and nothing else.");
    req.model = models
        .iter()
        .find(|m| m.id == "composer-2.5" || m.id.starts_with("composer-2.5"))
        .map(|m| m.id.clone());
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let stream = harness.run(req, controls).await.expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(180), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("ACP-OK"), "unexpected reply: {text:?}");
    assert_eq!(dones(&events).len(), 1, "{events:?}");
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
}

/// Cursor's todo extension reaches the stream as a chip, and its tools land
/// through the standard `session/update` path. Worth pinning live: the docs
/// call `cursor/update_todos` a fire-and-forget notification, but the CLI
/// sends it as a REQUEST — an unanswered one would stall the turn. Run:
/// `cargo test -p komet-harness --test acp -- --ignored real_cursor_todos`
#[tokio::test]
#[ignore = "needs the cursor-agent CLI authenticated + network; costs one small prompt"]
async fn real_cursor_todos_and_tools_reach_the_stream() {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::cursor();
    let mut req = request(
        "Use your todo tool to record 3 steps, then run `echo hi` in the shell. \
         Keep it brief.",
    );
    req.reasoning = None;
    req.cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let stream = harness.run(req, controls).await.expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(240), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let todos: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCall {
                    call: komet_proto::ToolCall::Todo { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(!todos.is_empty(), "no todo chip: {events:?}");
    // Repeated updates refresh one chip rather than stacking new ones.
    assert!(
        todos.iter().all(|e| matches!(
            e,
            AgentEvent::ToolCall { id, .. } if id == "cursor-todos"
        )),
        "todo chips must share the stable id: {todos:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall {
                call: komet_proto::ToolCall::Exec { .. },
                ..
            }
        )),
        "no exec tool call: {events:?}"
    );
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
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

#[test]
fn cursor_descriptor_surface_matches_registry_expectations() {
    let cursor = AcpHarness::cursor();
    assert_eq!(cursor.id(), HarnessId::Cursor);
    assert_eq!(cursor.display_name(), "Cursor");
    assert!(cursor.supports_steering());
    assert_eq!(cursor.steering_mode(), SteeringMode::TurnBoundary);
    // Cursor carries effort in the model id's bracket suffix, so there is no
    // separate ladder for the Reasoning dropdown to drive.
    assert!(cursor.reasoning_levels().is_empty());
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
