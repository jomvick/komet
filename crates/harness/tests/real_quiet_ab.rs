//! A/B evidence probe for the Claude quiet-settle exemption, against the
//! REAL stack (claude-agent-acp + claude CLI + live model). Run explicitly:
//!
//!   KOMET_AB=1 cargo test -p komet-harness --test real_quiet_ab -- --ignored --nocapture
//!
//! The knob is set to 800ms — far below routine inference-gap silence
//! (tool result → next API roundtrip), the same structural ratio as the
//! production incident (30s window vs 30–120s thinking stretches). On code
//! where Claude honors the blanket quiet settle, the probe false-settles
//! mid-turn: a premature Done{completed} with the turn's real tail (tool
//! calls / text) streaming AFTER it — the orphan signature. On code with
//! the exemption, the SAME knob setting must produce exactly one Done,
//! ordered after all content.
//!
//! The test asserts nothing: it prints a timestamped trace and a VERDICT
//! line, and is run on both trees so the traces can be compared.

use std::sync::Once;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use komet_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use komet_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion};

const QUIET_MS: u64 = 800;
/// How long to keep observing after the FIRST Done — on unfixed code the
/// orphaned turn's tail lands in this window.
const POST_DONE_WINDOW: Duration = Duration::from_secs(25);

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set before any harness runs in this test process.
        unsafe { std::env::set_var("KOMET_ACP_QUIET_SETTLE_MS", QUIET_MS.to_string()) };
    });
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions: Vec<UserInputQuestion>| {
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

fn brief(ev: &AgentEvent) -> String {
    match ev {
        AgentEvent::SessionStarted { session_id, .. } => format!("SessionStarted({session_id})"),
        AgentEvent::TextDelta { text } => {
            format!("TextDelta({:?})", text.chars().take(40).collect::<String>())
        }
        AgentEvent::ToolCall { id, .. } => format!("ToolCall({id})"),
        AgentEvent::ToolResult { id, .. } => format!("ToolResult({id})"),
        AgentEvent::AssistantMessageCompleted { .. } => "AssistantMessageCompleted".into(),
        AgentEvent::Done { status, .. } => format!("*** DONE({status:?}) ***"),
        other => {
            let dbg = format!("{other:?}");
            dbg.chars().take(60).collect()
        }
    }
}

#[tokio::test]
#[ignore = "real claude CLI + network; run explicitly for the A/B evidence probe"]
async fn real_claude_quiet_ab_probe() {
    init_env();
    let runs: usize = std::env::var("AB_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut clean = 0usize;
    let mut orphaned = 0usize;
    for run in 1..=runs {
        let (dones, after, finished) = probe_once(run == 1).await;
        println!(
            "RUN {run}/{runs} VERDICT: dones={dones} content_events_after_first_done={after} \
             finished_text_contains_PROBE_DONE={finished}"
        );
        if after == 0 && dones == 1 {
            clean += 1;
        } else {
            orphaned += 1;
        }
    }
    println!(
        "SUMMARY: runs={runs} clean={clean} orphaned={orphaned} \
         (clean = one Done, ordered last; orphaned = premature Done, tail after it)"
    );
}

async fn probe_once(print_trace: bool) -> (usize, usize, bool) {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::claude();
    let req = RunRequest {
        prompt: "Use your shell tool to run `echo probe-one`. After you see its output, \
                 run `echo probe-two` as a second separate command. After that, reply \
                 with exactly the word PROBE-DONE."
            .into(),
        harness: None,
        model: Some("claude-haiku-4-5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    };
    let started = std::time::Instant::now();
    let mut stream = harness.run(req, controls).await.expect("run starts");

    let mut events: Vec<(Duration, AgentEvent)> = Vec::new();
    let mut steer = Some(steer_tx);
    let mut first_done_at: Option<std::time::Instant> = None;
    loop {
        let budget = match first_done_at {
            // Post-Done observation: wait out the window, then release the
            // mailbox so the run can end.
            Some(at) => {
                let left = POST_DONE_WINDOW.saturating_sub(at.elapsed());
                if left.is_zero() && steer.is_some() {
                    steer = None;
                }
                left.max(Duration::from_secs(10))
            }
            None => Duration::from_secs(90),
        };
        match tokio::time::timeout(budget, stream.next()).await {
            Ok(Some(ev)) => {
                let ev = ev.expect("stream event");
                if matches!(ev, AgentEvent::Done { .. }) && first_done_at.is_none() {
                    first_done_at = Some(std::time::Instant::now());
                }
                events.push((started.elapsed(), ev));
            }
            Ok(None) => break,
            Err(_) => {
                if steer.is_some() {
                    steer = None; // release mailbox, drain to stream end
                } else {
                    break;
                }
            }
        }
        if events.len() > 400 {
            break;
        }
    }

    if print_trace {
        println!("--- TRACE (quiet knob = {QUIET_MS}ms) ---");
        for (at, ev) in &events {
            println!("{:>8.3}s  {}", at.as_secs_f64(), brief(ev));
        }
    }
    let first_done_idx = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }));
    let dones = events
        .iter()
        .filter(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .count();
    let after = first_done_idx
        .map(|i| {
            events[i + 1..]
                .iter()
                .filter(|(_, e)| {
                    matches!(
                        e,
                        AgentEvent::TextDelta { .. }
                            | AgentEvent::ToolCall { .. }
                            | AgentEvent::ToolResult { .. }
                    )
                })
                .count()
        })
        .unwrap_or(0);
    let text: String = events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (dones, after, text.contains("PROBE-DONE"))
}
