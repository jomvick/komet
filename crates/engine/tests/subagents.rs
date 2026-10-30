//! Task 4 (agent transparency): a subagent's transcript is a projection of the
//! host's run journal — no child doc, no child content in the parent's doc, and
//! the parent's chip moves only at the terminal write.
//!
//! These tests drive the real pipeline (doc command → host executor → harness
//! stream → journal + broadcast + folded doc) with scripted harnesses, so what
//! they assert is what a UI would read back.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use komet_doc::{
    CommandBasedOn, MessagePart, MessageRole, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus,
};
use komet_engine::{EngineCore, HarnessRegistry};
use komet_harness::{Harness, HarnessError, RunControls, mock::subagent_nested_script};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SteeringMode, ToolCall,
};

const CHAT: &str = "chat-subagents";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
        mcp: None,
        mcp_external: Vec::new(),
    }
}

fn session_started() -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-1".into(),
        assistant_message_id: "a-1".into(),
    }
}

fn done() -> AgentEvent {
    AgentEvent::Done {
        status: DoneStatus::Completed,
        result: None,
        error: None,
        session_id: Some("hs-1".into()),
        reason: None,
    }
}

fn text(s: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: s.into() }
}

/// Wrap one child's event in its DIRECT parent's feed, the shape the harnesses
/// emit (`parent` = the spawner's tool id).
fn child(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.into(),
        event: Box::new(event),
    }
}

fn spawn(id: &str, label: &str) -> AgentEvent {
    AgentEvent::ToolCall {
        id: id.into(),
        call: ToolCall::Unknown {
            name: format!("Agent: {label}"),
            input: None,
        },
    }
}

/// The harness trait needs a `Send + Sync` object; the script lives behind a
/// take-once slot so one run owns the stream.
struct ScriptedHarness {
    script: std::sync::Mutex<Option<Vec<AgentEvent>>>,
}

impl ScriptedHarness {
    fn new(script: Vec<AgentEvent>) -> Arc<Self> {
        Arc::new(Self {
            script: std::sync::Mutex::new(Some(script)),
        })
    }
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Scripted"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[komet_proto::ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<komet_proto::Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let script = self
            .script
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| HarnessError::NotInstalled("scripted harness reused".into()))?;
        Ok(futures::stream::iter(script.into_iter().map(Ok::<_, HarnessError>)).boxed())
    }
}

/// A harness that emits whatever the test hands it, so a run can be held
/// mid-stream while a subscription is taken out.
struct FedHarness {
    next: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<AgentEvent>>>,
}

impl FedHarness {
    /// `(harness, sender)` — closing the sender ends the stream.
    fn new() -> (Arc<Self>, tokio::sync::mpsc::Sender<AgentEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(32);
        (
            Arc::new(Self {
                next: std::sync::Mutex::new(Some(rx)),
            }),
            tx,
        )
    }
}

#[async_trait]
impl Harness for FedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Fed"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[komet_proto::ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<komet_proto::Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let rx = self
            .next
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| HarnessError::NotInstalled("fed harness reused".into()))?;
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

fn assemble(dir: &std::path::Path, harness: Arc<dyn Harness>) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

/// Queue a run the way the composer does; the host executor drains it.
fn queue_run(core: &EngineCore, id: &str, prompt: &str) {
    let handle = core.doc_host.open(CHAT).expect("open chat");
    let doc = handle.doc();
    let based_on = doc
        .read_entries()
        .expect("read entries")
        .last()
        .map(|entry| CommandBasedOn {
            turn_id: Some(entry.id.clone()),
            frontier: None,
        });
    doc.queue_command(&SessionCommandEntry {
        id: id.into(),
        payload: SessionCommandPayload::Run {
            request: run_request(prompt),
            message_id: format!("msg-{id}"),
        },
        issued_by: core.device_id.clone(),
        issued_at: chrono::Utc::now().timestamp_millis(),
        based_on,
        expires_at: None,
        status: SessionCommandStatus::Pending,
        resolution: None,
    })
    .expect("queue command");
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Every durable frame of the chat, as the journal holds it.
fn journaled(core: &EngineCore) -> Vec<(u64, AgentEvent)> {
    core.sessions
        .subscribe(CHAT, 0)
        .expect("subscribe")
        .0
        .into_iter()
        .map(|frame| (frame.seq, frame.event))
        .collect()
}

fn settled(core: &EngineCore) -> bool {
    journaled(core)
        .last()
        .is_some_and(|(_, event)| matches!(event, AgentEvent::Done { .. }))
}

/// What the parent's doc actually holds, once the run settled.
fn parent_parts(core: &EngineCore) -> Vec<MessagePart> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read entries")
        .into_iter()
        .filter(|entry| entry.role == MessageRole::Assistant)
        .flat_map(|entry| entry.parts)
        .collect()
}

/// Step 1: nested traffic is journaled and projected, and none of it lands in
/// the parent's doc.
#[tokio::test]
async fn subagent_events_are_journaled_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let script = std::iter::once(session_started())
        .chain(subagent_nested_script())
        .collect();
    let core = assemble(dir.path(), ScriptedHarness::new(script));
    queue_run(&core, "cmd-1", "delegate two levels");
    wait_for(|| settled(&core), "the run to settle").await;

    // A's feed, in journal order: its own prose, the chip that spawned B, and
    // the result it got back from B.
    let feed = core
        .sessions
        .subagent_events(CHAT, "mock-spawn-a")
        .expect("the journal is on this host");
    assert!(
        feed.iter().any(|(_, event)| matches!(
            event, AgentEvent::TextDelta { text } if text == "Root cause: the row fingerprint missed the new fields.\n\n"
        )),
        "A's own prose is on A's feed: {feed:?}"
    );
    assert!(
        feed.iter().any(|(_, event)| matches!(
            event, AgentEvent::ToolCall { id, .. } if id == "mock-spawn-b"
        )),
        "the chip that spawned B belongs to A: {feed:?}"
    );
    let seqs: Vec<_> = feed.iter().map(|(seq, _)| *seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the projection preserves journal order: {seqs:?}"
    );

    // The parent's doc saw the spawn and nothing else: no child tool chip, no
    // child prose. The journal is the only copy of a child's content.
    let parts = parent_parts(&core);
    let parent_text: String = parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        parent_text.contains("Two levels of delegation"),
        "the parent's own prose is folded: {parent_text:?}"
    );
    assert!(
        !parent_text.contains("Root cause: the row fingerprint")
            && !parent_text.contains("The fold keys each part by id"),
        "no child prose reached the parent doc: {parent_text:?}"
    );
    assert!(
        !parts.iter().any(|part| matches!(
            part, MessagePart::Tool { id, .. } if id == "mock-bash-b"
        )),
        "B's tool call must not mint a chip in the parent's transcript"
    );
    let chip = parts
        .iter()
        .find(|part| matches!(part, MessagePart::Tool { id, .. } if id == "mock-spawn-a"))
        .expect("the spawn chip for A");
    match chip {
        MessagePart::Tool {
            call,
            subagent_status,
            ..
        } => {
            assert!(call.is_subagent_spawn(), "the chip is a spawn");
            assert_eq!(
                *subagent_status,
                Some(komet_doc::SubagentStatus::Done),
                "the chip settles from A's own Done"
            );
        }
        other => panic!("unexpected chip {other:?}"),
    }
}

/// Step 2: a sub-subagent stays under the feed that spawned it.
#[tokio::test]
async fn nested_subagents_stay_under_their_direct_parent() {
    let dir = tempfile::tempdir().unwrap();
    let script = vec![
        session_started(),
        text("root: handing off\n\n"),
        spawn("spawn-a", "investigate"),
        child("spawn-a", text("a: delegating the read\n\n")),
        child("spawn-a", spawn("spawn-b", "read sessions.rs")),
        child("spawn-b", text("b: the fold keys parts by id\n\n")),
        child(
            "spawn-b",
            AgentEvent::ToolCall {
                id: "b-tool".into(),
                call: ToolCall::Exec {
                    command: "sed -n 1,120p sessions.rs".into(),
                },
            },
        ),
        child("spawn-a", done()),
        done(),
    ];
    let core = assemble(dir.path(), ScriptedHarness::new(script));
    queue_run(&core, "cmd-1", "two levels");
    wait_for(|| settled(&core), "the run to settle").await;

    let a = core.sessions.subagent_events(CHAT, "spawn-a").unwrap();
    let b = core.sessions.subagent_events(CHAT, "spawn-b").unwrap();
    let a_text: Vec<_> = a
        .iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        a_text,
        vec!["a: delegating the read\n\n".to_string()],
        "A's feed holds A's prose only — B's content stays under B"
    );
    assert!(
        a.iter()
            .any(|(_, event)| matches!(event, AgentEvent::ToolCall { id, .. } if id == "spawn-b")),
        "A sees the chip that spawned B"
    );
    assert!(
        !a.iter()
            .any(|(_, event)| matches!(event, AgentEvent::ToolCall { id, .. } if id == "b-tool")),
        "B's tool is not A's traffic"
    );
    assert_eq!(b.len(), 2, "B's prose and its tool call: {b:?}");
    // A settles with its OWN Done (that is what flips its chip in the parent
    // doc); B was never settled here, so nothing about B leaks upward.
    assert!(
        matches!(
            a.last().map(|(_, event)| event),
            Some(AgentEvent::Done { .. })
        ),
        "the child settles its own feed: {a:?}"
    );
    assert!(
        b.iter()
            .all(|(_, event)| !matches!(event, AgentEvent::Done { .. })),
        "B never settled, and A's Done is not B's traffic: {b:?}"
    );
}

/// Step 4: the subagent view is subscribe-then-replay — what was already
/// journaled arrives as the replay, what arrives afterwards on the live stream,
/// and the two agree on `seq`, so a viewer can dedupe without a hole.
#[tokio::test]
async fn watch_subagent_subscribes_then_replays() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, tx) = FedHarness::new();
    let core = assemble(dir.path(), harness);
    queue_run(&core, "cmd-1", "stream me a child");

    tx.send(session_started()).await.unwrap();
    tx.send(text("root\n\n")).await.unwrap();
    tx.send(child("spawn-a", text("a one\n\n"))).await.unwrap();
    wait_for(
        || {
            core.sessions
                .subagent_events(CHAT, "spawn-a")
                .is_some_and(|feed| feed.len() == 1)
        },
        "A's first frame to reach the journal",
    )
    .await;

    let (replay, mut live) = core
        .sessions
        .subscribe_subagent(CHAT, "spawn-a", 0)
        .expect("subscribe");
    let replay = replay.expect("this host ran the chat");
    assert_eq!(replay.len(), 1, "the already-journaled frame replays");
    let replayed_seq = replay[0].seq;

    // The child keeps producing once the subscription exists: live traffic, not
    // a second copy.
    tx.send(child("spawn-a", text("a two\n\n"))).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), live.recv())
        .await
        .expect("the live frame arrives")
        .expect("the hub stays open");
    let AgentEvent::Subagent {
        parent_tool_use_id,
        event,
    } = &frame.event
    else {
        panic!("the live stream is the chat's own feed: {:?}", frame.event)
    };
    assert_eq!(parent_tool_use_id, "spawn-a");
    assert!(matches!(**event, AgentEvent::TextDelta { .. }));
    assert!(
        frame.seq > replayed_seq,
        "live seqs follow the replay, which is what makes dedupe by seq safe: {} vs {replayed_seq}",
        frame.seq
    );

    tx.send(child("spawn-a", done())).await.unwrap();
    tx.send(done()).await.unwrap();
    drop(tx);
    wait_for(|| settled(&core), "the run to settle").await;

    // Nothing lost, nothing doubled: replay ∪ live covers exactly the feed the
    // journal ends up holding. The live stream is the CHAT's, so only the
    // frames wrapping A's traffic count.
    let mut live_seqs = vec![frame.seq];
    while let Ok(frame) = live.try_recv() {
        if let AgentEvent::Subagent {
            parent_tool_use_id, ..
        } = &frame.event
            && parent_tool_use_id == "spawn-a"
        {
            live_seqs.push(frame.seq);
        }
    }
    let feed = core.sessions.subagent_events(CHAT, "spawn-a").unwrap();
    assert_eq!(feed.len(), 3, "every A frame the journal holds: {feed:?}");
    let covered: std::collections::HashSet<u64> = replay
        .iter()
        .map(|frame| frame.seq)
        .chain(live_seqs.iter().copied())
        .collect();
    assert_eq!(
        covered,
        feed.iter()
            .map(|(seq, _)| *seq)
            .collect::<std::collections::HashSet<_>>(),
        "replay ∪ live is the whole feed (live saw {live_seqs:?})"
    );
    assert!(
        live_seqs.len() >= 2,
        "the frames appended after the subscription arrived live, not only in the journal: {live_seqs:?}"
    );
}

/// Step 5: a chat this host never ran answers "no transcript here", never an
/// empty one.
#[tokio::test]
async fn subagent_transcript_without_journal_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        ScriptedHarness::new(vec![session_started(), done()]),
    );
    assert!(
        core.sessions
            .subagent_events("chat-never-run-here", "spawn-a")
            .is_none(),
        "no journal on this host is `unavailable`, not an empty feed"
    );
    let (replay, _live) = core
        .sessions
        .subscribe_subagent("chat-never-run-here", "spawn-a", 0)
        .expect("a subscription is still openable — a run may start here");
    assert!(
        replay.is_none(),
        "a host with no journal says so, instead of opening a blank transcript"
    );
}

/// Step 6b: deleting the chat takes its journal (and its revival budget) with
/// it, and nothing brings the transcript back.
#[tokio::test]
async fn deleting_a_chat_discards_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let script = std::iter::once(session_started())
        .chain(subagent_nested_script())
        .collect();
    let core = assemble(dir.path(), ScriptedHarness::new(script));
    queue_run(&core, "cmd-1", "delegate");
    wait_for(|| settled(&core), "the run to settle").await;
    assert!(
        core.sessions
            .subagent_events(CHAT, "mock-spawn-b")
            .is_some_and(|feed| !feed.is_empty()),
        "the child's transcript exists before the delete"
    );

    // Exactly what the DeleteChat RPC does.
    core.workspace.delete_chat(CHAT).expect("delete chat");
    core.doc_host.purge_chat(CHAT);

    assert!(
        core.sessions
            .subagent_events(CHAT, "mock-spawn-b")
            .is_none(),
        "the journal died with the chat"
    );
    assert!(
        core.sessions.subscribe(CHAT, 0).unwrap().0.is_empty(),
        "no replay survives a deleted chat"
    );
    assert_eq!(core.sessions.latest_seq(CHAT), 0);
    // No resurrection: opening the chat again is a fresh doc, not the deleted
    // transcript re-read from a snapshot or a leftover journal.
    let entries = core
        .doc_host
        .open(CHAT)
        .expect("reopen")
        .doc()
        .read_entries()
        .expect("read entries");
    assert!(
        entries.is_empty(),
        "a deleted chat reopens empty: {entries:?}"
    );
    assert_eq!(
        core.sessions.recover_stale().expect("stale scan"),
        0,
        "a deleted chat is not a crash to recover"
    );
}

/// Step 6c: only durable facts grow the journal — a tool's live output tail
/// stays on the broadcast, at any nesting level.
#[tokio::test]
async fn tool_progress_and_activity_are_not_journaled() {
    let dir = tempfile::tempdir().unwrap();
    let script = vec![
        session_started(),
        text("root\n\n"),
        AgentEvent::ToolProgress {
            id: "root-tool".into(),
            text: "half a log line\n".into(),
            append: true,
        },
        child(
            "spawn-a",
            AgentEvent::ToolProgress {
                id: "a-tool".into(),
                text: "a child printing forever\n".into(),
                append: true,
            },
        ),
        child(
            "spawn-a",
            AgentEvent::ToolResult {
                id: "a-tool".into(),
                is_error: false,
                output: Some("done".into()),
                diff: None,
            },
        ),
        done(),
    ];
    let core = assemble(dir.path(), ScriptedHarness::new(script));
    queue_run(&core, "cmd-1", "print a lot");
    wait_for(|| settled(&core), "the run to settle").await;

    let journal = journaled(&core);
    assert!(
        !journal
            .iter()
            .any(|(_, event)| matches!(event, AgentEvent::ToolProgress { .. })),
        "a top-level progress frame never reaches the journal"
    );
    assert!(
        !journal.iter().any(|(_, event)| matches!(
            event,
            AgentEvent::Subagent { event, .. } if matches!(**event, AgentEvent::ToolProgress { .. })
        )),
        "nor does a child's — that is the frame a long subagent run streams"
    );
    assert!(
        journal.iter().any(|(_, event)| matches!(
            event,
            AgentEvent::Subagent { event, .. } if matches!(**event, AgentEvent::ToolResult { .. })
        )),
        "the child's settled result is a durable fact and does get journaled"
    );
    // The child's feed is the durable fact alone, with no tail noise in it.
    let feed = core.sessions.subagent_events(CHAT, "spawn-a").unwrap();
    assert_eq!(feed.len(), 1, "{feed:?}");
}

/// Step 3 (the doc half of the transparency contract): a subagent's cycle costs
/// the parent doc ONE write, at the terminal event. Mid-run the chip carries its
/// lifecycle and no numbers at all — a live count that rode the doc would reopen
/// the oplog per tool, which is exactly what the journal projection replaced.
#[tokio::test]
async fn parent_chip_is_written_once_at_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, feed) = FedHarness::new();
    let core = assemble(dir.path(), harness);
    queue_run(&core, "cmd-1", "delegate one child");

    feed.send(session_started()).await.unwrap();
    feed.send(spawn("spawn-a", "scan the tree")).await.unwrap();
    feed.send(child("spawn-a", text("reading"))).await.unwrap();
    feed.send(child(
        "spawn-a",
        AgentEvent::ToolCall {
            id: "a-bash".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        },
    ))
    .await
    .unwrap();

    // The child is live in the doc — running, and carrying nothing else.
    wait_for(
        || match chip_of(&core, "spawn-a") {
            Some((status, _, _)) => status == Some(komet_doc::SubagentStatus::Running),
            None => false,
        },
        "the spawn chip to reach Running",
    )
    .await;
    let (status, duration, count) = chip_of(&core, "spawn-a").expect("chip");
    assert_eq!(status, Some(komet_doc::SubagentStatus::Running));
    assert_eq!(
        (duration, count),
        (None, None),
        "a cycle still in flight has no terminal numbers in the doc"
    );

    // The child settles: one write, with both numbers in it.
    feed.send(child(
        "spawn-a",
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            reason: None,
        },
    ))
    .await
    .unwrap();
    wait_for(
        || matches!(chip_of(&core, "spawn-a"), Some((_, Some(_), Some(_)))),
        "the terminal write",
    )
    .await;
    let (status, Some(duration), Some(count)) = chip_of(&core, "spawn-a").expect("chip") else {
        unreachable!()
    };
    assert_eq!(status, Some(komet_doc::SubagentStatus::Done));
    assert_eq!(count, 1, "only the child's own tool counts: {status:?}");
    assert!(
        duration < 60_000,
        "the cycle is measured on the host clock, not invented: {duration} ms"
    );

    // The run keeps going and ends: the chip's numbers do not move again.
    feed.send(text("root wrap-up")).await.unwrap();
    feed.send(done()).await.unwrap();
    drop(feed);
    wait_for(|| settled(&core), "the run to settle").await;
    let (_, after_duration, after_count) = chip_of(&core, "spawn-a").expect("chip");
    assert_eq!(
        (after_duration, after_count),
        (Some(duration), Some(count)),
        "the terminal write is the only one"
    );
}

fn chip_of(
    core: &EngineCore,
    id: &str,
) -> Option<(Option<komet_doc::SubagentStatus>, Option<u64>, Option<u64>)> {
    parent_parts(core).into_iter().find_map(|part| match part {
        MessagePart::Tool {
            id: pid,
            subagent_status,
            subagent_duration_ms,
            subagent_tool_count,
            ..
        } if pid == *id => Some((subagent_status, subagent_duration_ms, subagent_tool_count)),
        _ => None,
    })
}

/// Step 5 at the RPC boundary: the transcript is answered from the host's
/// journal, and a device with nothing to project says so instead of opening an
/// empty feed.
#[tokio::test]
async fn watch_subagent_rpc_answers_from_the_hosts_journal() {
    use futures::StreamExt as _;
    use komet_rpc::{RpcReply, RpcService as _, methods};

    let dir = tempfile::tempdir().unwrap();
    let script = std::iter::once(session_started())
        .chain(subagent_nested_script())
        .collect();
    let core = assemble(dir.path(), ScriptedHarness::new(script));
    queue_run(&core, "cmd-1", "delegate two levels");
    wait_for(|| settled(&core), "the run to settle").await;
    let rpc = core.rpc_service();

    let reply = rpc
        .handle(
            methods::WATCH_SUBAGENT,
            serde_json::json!({ "chatId": CHAT, "parentToolUseId": "mock-spawn-a" }),
        )
        .await
        .expect("watch subagent");
    let RpcReply::Stream(mut stream) = reply else {
        panic!("WatchSubagent answers with a stream");
    };
    let head = stream.next().await.expect("header frame");
    assert_eq!(head["status"], "ok", "{head}");
    assert!(
        head["replayed"].as_u64().is_some_and(|n| n > 0),
        "the journaled feed is replayed from the start: {head}"
    );
    let first = stream.next().await.expect("first replay frame");
    assert_eq!(first["phase"], "replay", "{first}");
    assert_eq!(
        first["event"]["type"].as_str(),
        Some("textDelta"),
        "frames are the child's own events, unwrapped: {first}"
    );
    drop(stream);

    // A chat this host never ran: an honest banner, and nothing behind it.
    let reply = rpc
        .handle(
            methods::WATCH_SUBAGENT,
            serde_json::json!({ "chatId": "chat-never-ran", "parentToolUseId": "mock-spawn-a" }),
        )
        .await
        .expect("watch subagent");
    let RpcReply::Stream(mut stream) = reply else {
        panic!("WatchSubagent answers with a stream");
    };
    let head = stream.next().await.expect("banner");
    assert_eq!(head["status"], "unavailable", "{head}");
    assert!(
        head["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "and it says why: {head}"
    );
    assert!(
        stream.next().await.is_none(),
        "an unavailable feed has no frames behind the banner"
    );
}
