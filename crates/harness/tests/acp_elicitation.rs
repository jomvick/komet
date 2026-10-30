//! Gate B coverage for the ACP draft `elicitation/create` request: the
//! advertisement is opt-in per agent, forms round-trip, and every refusal is
//! explicit on the wire.
//!
//! One test on purpose: the gate is process env, and this binary's tests would
//! otherwise race each other's `KOMET_ACP_ELICITATION`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use komet_proto::{
    AgentEvent, DoneStatus, QuestionSource, RunRequest, SandboxLevel, UserInputAnswer,
    UserInputQuestion,
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
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        resume: None,
        worktree: None,
        mcp: None,
        mcp_external: Vec::new(),
    }
}

type Asked = Arc<Mutex<Vec<(Vec<UserInputQuestion>, QuestionSource)>>>;

struct TestRun {
    controls: RunControls,
    asked: Asked,
    _keep: (mpsc::Sender<SteerMessage>, CancellationToken),
}

fn bridge(
    answer: impl Fn(&[UserInputQuestion]) -> Vec<UserInputAnswer> + Send + Sync + 'static,
) -> TestRun {
    let asked: Asked = Arc::default();
    let seen = Arc::clone(&asked);
    let answer: Arc<dyn Fn(&[UserInputQuestion]) -> Vec<UserInputAnswer> + Send + Sync> =
        Arc::new(answer);
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    TestRun {
        controls: RunControls {
            request_input: Box::new(move |questions, source| {
                seen.lock()
                    .expect("asked lock")
                    .push((questions.clone(), source));
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(answer(&questions));
                rx
            }),
            request_permission: RunControls::noop_permission(),
            steering: steer_rx,
            interrupt: token.clone(),
            env: Vec::new(),
        },
        asked,
        _keep: (steer_tx, token),
    }
}

async fn run(
    scenario: &str,
    answer: impl Fn(&[UserInputQuestion]) -> Vec<UserInputAnswer> + Send + Sync + 'static,
) -> (Vec<AgentEvent>, Asked) {
    let TestRun {
        controls,
        asked,
        _keep,
    } = bridge(answer);
    let stream = harness()
        .run(request(scenario), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .unwrap_or_else(|_| panic!("run finished in time: {scenario}"));
    drop(_keep);
    (events, asked)
}

fn texts(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn dones(events: &[AgentEvent]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

fn answer(question_id: &str, labels: &[&str]) -> UserInputAnswer {
    UserInputAnswer {
        question_id: question_id.into(),
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        free_text: None,
    }
}

#[tokio::test]
async fn gate_b_elicitation_forms_round_trip_and_refusals_are_explicit() {
    // Gate off: nothing was advertised, so a request that arrives anyway is
    // refused with -32601 — the user is never asked a form the agent did not
    // negotiate.
    let (events, asked) = run("scenario:elicit-refused", |_| Vec::new()).await;
    assert_eq!(texts(&events), ["refused"], "{events:?}");
    assert!(
        asked.lock().expect("asked lock").is_empty(),
        "a refused elicitation never reaches the input bridge"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);

    // Gate on for this agent only (comma-separated slugs; `grok` here).
    unsafe { std::env::set_var("KOMET_ACP_ELICITATION", "grok") };

    // Single select: the mapped question carries the schema's labels and
    // default, and the answer goes back as the schema VALUE.
    let (events, asked) = run("scenario:elicit-single", |_| {
        vec![answer("env", &["Production"])]
    })
    .await;
    {
        let asked = asked.lock().expect("asked lock");
        let (questions, source) = &asked[0];
        assert_eq!(*source, QuestionSource::Elicitation);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].id, "env");
        assert_eq!(questions[0].header, "Environment");
        assert_eq!(questions[0].question, "Pick the target");
        assert_eq!(questions[0].options, ["Staging", "Production"]);
        assert_eq!(questions[0].default.as_deref(), Some("Staging"));
    }
    assert_eq!(texts(&events), ["answered"], "{events:?}");
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);

    // Several properties → several questions in one request (the wizard
    // paginates them).
    let (events, _asked) = run("scenario:elicit-multi", |questions| {
        assert_eq!(questions.len(), 2);
        vec![answer("name", &["v2.1"]), answer("channel", &["Beta"])]
    })
    .await;
    assert_eq!(texts(&events), ["answered"], "{events:?}");

    // Array of enum → multi-select; picked labels come back as values.
    let (events, _asked) = run("scenario:elicit-array", |questions| {
        assert!(questions[0].multi_select);
        vec![answer("checks", &["Format", "Tests"])]
    })
    .await;
    assert_eq!(texts(&events), ["answered"], "{events:?}");

    // No answers = the user dismissed the form: cancel, never accept.
    let (events, _asked) = run("scenario:elicit-cancel", |_| Vec::new()).await;
    assert_eq!(texts(&events), ["cancelled"], "{events:?}");

    // An answer that does not satisfy a required field declines.
    let (events, _asked) = run("scenario:elicit-decline", |_| {
        vec![UserInputAnswer {
            question_id: "name".into(),
            labels: Vec::new(),
            free_text: None,
        }]
    })
    .await;
    assert_eq!(texts(&events), ["declined"], "{events:?}");

    // url mode is declared but unsupported: -32602, and the agent decides what
    // to do next (the fixture ends the turn only when it sees that code).
    let (events, asked) = run("scenario:elicit-url", |_| Vec::new()).await;
    assert_eq!(texts(&events), ["unsupported"], "{events:?}");
    assert!(
        asked.lock().expect("asked lock").is_empty(),
        "an unsupported mode is refused before a question is minted"
    );

    unsafe { std::env::remove_var("KOMET_ACP_ELICITATION") };
}
