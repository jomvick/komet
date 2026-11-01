//! Terminal read transport (plan step 4): the `ListTerminals` /
//! `SetTerminalAgentVisible` / `ReadTerminal*` RPC surface over the memory
//! transport, with the agent-visibility guard proven end to end — reads are
//! refused until the human grants them, answer with clear text once granted,
//! and go back to refusing after revocation. The last two tests prove the
//! BOUND on the agent birth grant: an agent-opened terminal is readable by
//! its creator run only (reads and listing), and the creator's
//! `terminal_run` → `running` + `sinceSeq` → delta follow-up still works.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use komet_engine::{EngineCore, HarnessRegistry, Terminals};
use komet_proto::{TerminalDelta, TerminalScreen, TerminalSession, TerminalTail};
use komet_rpc::methods;

fn assemble(dir: &Path) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    EngineCore::assemble(
        dir,
        Arc::new(HarnessRegistry::new()),
        komet_proto::HarnessId::Mock,
        None,
    )
    .expect("engine assembles")
}

/// Poll the gated screen read until the marker shows (or blow up loudly —
/// a read that never answers is exactly the silent no-op this forbids).
async fn wait_for_screen(
    client: &komet_rpc::RpcClient,
    terminal_id: &str,
    marker: &str,
) -> TerminalScreen {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let screen = client
            .call_as::<TerminalScreen>(
                methods::READ_TERMINAL_SCREEN,
                serde_json::json!({ "terminalId": terminal_id }),
            )
            .await
            .expect("granted screen read");
        if screen.lines.iter().any(|line| line.contains(marker)) {
            return screen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker {marker:?} never reached the mirrored screen: {:?}",
            screen.lines
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_terminal_reads_are_guarded_by_the_human_grant() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Pin a bare shell and keep Repos' worktree root inside the tempdir
    // (same normalization as the m5 terminal suite).
    unsafe { std::env::set_var("SHELL", "/bin/sh") };
    unsafe { std::env::set_var("KOMET_WORKTREES_DIR", tmp.path().join("worktrees")) };
    let core = assemble(&tmp.path().join("data"));
    let client = komet_rpc::memory_client(core.rpc_service());

    let session = client
        .call_as::<TerminalSession>(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "cols": 80, "rows": 24 }),
        )
        .await
        .expect("OpenTerminal");
    let id = session.id.clone();

    // Default deny: the row is listed for the human but flagged invisible,
    // and every read refuses — before the terminal even prints.
    let listed = client
        .call(methods::LIST_TERMINALS, serde_json::Value::Null)
        .await
        .expect("ListTerminals");
    let row = listed["terminals"]
        .as_array()
        .expect("terminals array")
        .iter()
        .find(|row| row["terminalId"] == serde_json::json!(id))
        .expect("opened terminal is listed");
    assert_eq!(row["agentVisible"], false);
    assert_eq!(row["cwd"], serde_json::json!(session.cwd));
    assert_eq!(row["shell"], "sh");
    assert_eq!(row["cols"], 80);
    assert_eq!(row["rows"], 24);

    for (method, params) in [
        (
            methods::READ_TERMINAL_SCREEN,
            serde_json::json!({ "terminalId": id }),
        ),
        (
            methods::READ_TERMINAL_TAIL,
            serde_json::json!({ "terminalId": id, "lines": 5 }),
        ),
        (
            methods::READ_TERMINAL_DELTA,
            serde_json::json!({ "terminalId": id, "sinceSeq": 0 }),
        ),
    ] {
        let err = client
            .call(method, params)
            .await
            .expect_err("an invisible terminal must be refused, never answered empty");
        assert!(
            err.to_string().contains("not visible to agents"),
            "{method}: {err}"
        );
    }

    // The human grants: tracking comes with it, and output becomes readable.
    client
        .call(
            methods::SET_TERMINAL_AGENT_VISIBLE,
            serde_json::json!({ "terminalId": id, "visible": true }),
        )
        .await
        .expect("grant");
    let listed = client
        .call(methods::LIST_TERMINALS, serde_json::Value::Null)
        .await
        .expect("ListTerminals");
    let row = listed["terminals"]
        .as_array()
        .expect("terminals array")
        .iter()
        .find(|row| row["terminalId"] == serde_json::json!(id))
        .expect("still listed");
    assert_eq!(row["agentVisible"], true);
    assert_eq!(row["tracked"], true, "the grant switched the mirror on");

    client
        .call(
            methods::WRITE_TERMINAL,
            serde_json::json!({
                "terminalId": id,
                "data": BASE64.encode("echo rpcgr0 ant\n"),
            }),
        )
        .await
        .expect("write echo");
    let screen = wait_for_screen(&client, &id, "rpcgr0 ant").await;
    assert_eq!(screen.terminal_id, id);
    assert_eq!(screen.rows, 24);

    let tail = client
        .call_as::<TerminalTail>(
            methods::READ_TERMINAL_TAIL,
            serde_json::json!({ "terminalId": id, "lines": 50 }),
        )
        .await
        .expect("granted tail read");
    assert_eq!(tail.requested, 50);
    assert!(
        tail.lines.iter().any(|line| line.contains("rpcgr0 ant")),
        "{:?}",
        tail.lines
    );

    let delta = client
        .call_as::<TerminalDelta>(
            methods::READ_TERMINAL_DELTA,
            serde_json::json!({ "terminalId": id, "sinceSeq": screen.seq }),
        )
        .await
        .expect("granted delta read");
    assert_eq!(delta.from_seq, screen.seq);
    assert!(delta.through_seq >= screen.seq);

    // Revocation: refused again, and the mirror is dropped (no clear-text
    // scrollback kept for a terminal agents cannot read).
    client
        .call(
            methods::SET_TERMINAL_AGENT_VISIBLE,
            serde_json::json!({ "terminalId": id, "visible": false }),
        )
        .await
        .expect("revoke");
    let err = client
        .call(
            methods::READ_TERMINAL_SCREEN,
            serde_json::json!({ "terminalId": id }),
        )
        .await
        .expect_err("revoked terminal is refused");
    assert!(err.to_string().contains("not visible to agents"), "{err}");
    let listed = client
        .call(methods::LIST_TERMINALS, serde_json::Value::Null)
        .await
        .expect("ListTerminals");
    let row = listed["terminals"]
        .as_array()
        .expect("terminals array")
        .iter()
        .find(|row| row["terminalId"] == serde_json::json!(id))
        .expect("still listed");
    assert_eq!(row["tracked"], false, "revoking dropped the mirror");

    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": id }),
        )
        .await
        .expect("CloseTerminal");
}

/// The gate is a per-terminal flag, not a global switch: granting one terminal
/// does not open the others (the human marks the agent's OWN terminals visible
/// one by one in step 6's UI).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_grant_is_per_terminal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe { std::env::set_var("SHELL", "/bin/sh") };
    unsafe { std::env::set_var("KOMET_WORKTREES_DIR", tmp.path().join("worktrees")) };
    let core = assemble(&tmp.path().join("data"));
    let client = komet_rpc::memory_client(core.rpc_service());

    let one = client
        .call_as::<TerminalSession>(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "cols": 80, "rows": 24 }),
        )
        .await
        .expect("open first");
    let other = client
        .call_as::<TerminalSession>(
            methods::OPEN_TERMINAL,
            serde_json::json!({ "cols": 80, "rows": 24 }),
        )
        .await
        .expect("open second");

    client
        .call(
            methods::SET_TERMINAL_AGENT_VISIBLE,
            serde_json::json!({ "terminalId": one.id, "visible": true }),
        )
        .await
        .expect("grant first");
    let err = client
        .call(
            methods::READ_TERMINAL_SCREEN,
            serde_json::json!({ "terminalId": other.id }),
        )
        .await
        .expect_err("the grant does not leak to the other terminal");
    assert!(err.to_string().contains("not visible to agents"), "{err}");

    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": one.id }),
        )
        .await
        .expect("close first");
    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": other.id }),
        )
        .await
        .expect("close second");
}

/// Poll a gated screen read until the marker shows (or blow up loudly — a
/// birth-granted terminal that never answers is the silent no-op again).
async fn wait_for_agent_screen(terminals: &Terminals, id: &str, marker: &str) -> TerminalScreen {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let screen = terminals
            .agent_screen_text_for_run(id, Some("run-a"))
            .expect("creator-run gated screen read");
        if screen.lines.iter().any(|line| line.contains(marker)) {
            return screen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker {marker:?} never reached the creator-readable screen: {:?}",
            screen.lines
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The bounded birth grant, end to end: a terminal `terminal_open` creates
/// for run A is readable and listable by run A WITHOUT any human click (the
/// grant at birth is kept — otherwise `terminal_run` → delta would need a
/// click per terminal), but run B is refused on every angle AND does not see
/// the row in the agent listing (title/cwd leak as little as scrollback).
/// The human's own terminal keeps the strict rule: nothing without
/// `SetTerminalAgentVisible` — and when the human DOES grant the agent's
/// terminal, that explicit grant opens it to every run, boundary lifted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_run_cannot_read_or_list_a_terminal_run_a_opened() {
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe { std::env::set_var("SHELL", "/bin/sh") };
    let terminals = Terminals::new();
    let dir = tmp.path().to_string_lossy().to_string();

    let mine = terminals
        .open_agent_terminal(Some(&dir), 80, 24, Some("run-a"))
        .expect("terminal_open for run-a");
    let human = terminals
        .open(&dir, 80, 24)
        .expect("human open (no run identity)");

    // Run A reads its own terminal with no human involved: the birth grant,
    // bounded to the creator.
    terminals
        .write(&mine.id, &BASE64.encode("echo cr0ator read\n"))
        .expect("write");
    let screen = wait_for_agent_screen(&terminals, &mine.id, "cr0ator read").await;
    assert!(!screen.lines.is_empty());
    assert!(
        terminals
            .agent_tail_text_for_run(&mine.id, 200, Some("run-a"))
            .expect("creator tail")
            .lines
            .iter()
            .any(|line| line.contains("cr0ator read")),
        "the creator run reads its own tail"
    );
    let _ = terminals
        .agent_text_delta_for_run(&mine.id, screen.seq, Some("run-a"))
        .expect("creator delta");

    // Run B: refused on every angle, with the bound said out loud.
    let refusals = [
        terminals
            .agent_screen_text_for_run(&mine.id, Some("run-b"))
            .map(|_| ()),
        terminals
            .agent_tail_text_for_run(&mine.id, 10, Some("run-b"))
            .map(|_| ()),
        terminals
            .agent_text_delta_for_run(&mine.id, 0, Some("run-b"))
            .map(|_| ()),
        terminals
            .agent_last_command_for_run(&mine.id, Some("run-b"))
            .map(|_| ()),
    ];
    for result in refusals {
        let err = result.expect_err("run B must not read run A's terminal");
        assert!(
            err.to_string().contains("opened by agent run run-a"),
            "the refusal names the bound, got: {err}"
        );
    }
    // The human/IPC surface passes no run identity: it too is refused until
    // a human grant (None never matches a recorded owner).
    let err = terminals
        .agent_screen_text(&mine.id)
        .expect_err("an ungranted, creator-bounded terminal refuses a reader with no identity");
    assert!(
        err.to_string().contains("opened by agent run run-a"),
        "{err}"
    );

    // Listing is gated exactly like reading: run A sees its own agent
    // terminal, run B never learns it exists (id, title, cwd and all).
    let listed_for = |reader: Option<&str>| -> Vec<String> {
        terminals
            .list_terminals_for_reader(reader)
            .into_iter()
            .map(|info| info.terminal_id)
            .collect()
    };
    assert!(listed_for(Some("run-a")).contains(&mine.id));
    assert!(
        !listed_for(Some("run-b")).contains(&mine.id),
        "terminal_list must not leak another run's terminal"
    );
    assert!(!listed_for(None).contains(&mine.id));
    // The human's ungranted terminal is invisible to every run, with the
    // ORIGINAL refusal (the per-terminal human grant is untouched).
    for reader in [Some("run-a"), Some("run-b"), None] {
        assert!(!listed_for(reader).contains(&human.id));
        let err = terminals
            .agent_screen_text_for_run(&human.id, reader)
            .expect_err("an invisible human terminal stays refused");
        assert!(
            err.to_string().contains("not visible to agents"),
            "human terminals keep the strict gate message: {err}"
        );
    }
    // The raw list stays the human/UI surface: everything is there, flagged.
    let raw = terminals.list_terminals();
    assert!(raw.iter().any(|info| info.terminal_id == mine.id));
    assert!(raw.iter().any(|info| info.terminal_id == human.id));

    // The explicit human grant overrides the bound (clause 1 of the gate):
    // once a human marks run A's terminal visible, run B reads and lists it.
    terminals
        .set_agent_visible(&mine.id, true)
        .expect("human grants run A's terminal");
    terminals
        .agent_screen_text_for_run(&mine.id, Some("run-b"))
        .expect("an explicit human grant opens the terminal to every run");
    assert!(listed_for(Some("run-b")).contains(&mine.id));

    terminals
        .close_agent_terminal(&mine.id)
        .expect("close agent one");
    terminals.close(&human.id).expect("close human shell");
}

/// The wanted flow survives the bound: `terminal_run` on the agent's own
/// terminal answers `running` with a `sinceSeq` anchor, and the SAME run
/// follows it with a gated delta read that grows until the command prints —
/// no human click anywhere in between. Another run following the same
/// anchor is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_creator_run_follows_a_running_answer_with_its_delta_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe { std::env::set_var("SHELL", "/bin/sh") };
    let terminals = Terminals::new();

    let mine = terminals
        .open_agent_terminal(
            Some(&tmp.path().to_string_lossy()),
            80,
            24,
            Some("run-flow"),
        )
        .expect("terminal_open for run-flow");
    // A command that outlives the wait: the honest `running` + anchor answer,
    // not a timeout, not a hang. The long silence bound keeps the marker-
    // free fallback from settling the stretch early.
    let answer = terminals
        .agent_run(
            &mine.id,
            "sleep 2 && echo fl0w delta",
            Duration::from_millis(400),
            Duration::from_secs(30),
        )
        .await
        .expect("agent_run answers, bounded");
    assert_eq!(answer.status, "running", "{answer:?}");
    assert_eq!(answer.terminal_id, mine.id);
    let since_seq = answer.since_seq;

    // The creator run reads the delta from the anchor — no grant was asked
    // of a human, the birth grant is bounded to exactly this run.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let delta = terminals
            .agent_text_delta_for_run(&mine.id, since_seq, Some("run-flow"))
            .expect("creator-run delta read after a running answer");
        if delta.lines.iter().any(|line| line.contains("fl0w delta")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delta never carried the command's output: {:?}",
            delta.lines
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Another run, holding the same anchor, is refused: the follow-up is
    // readable only by the run that opened the terminal.
    let err = terminals
        .agent_text_delta_for_run(&mine.id, since_seq, Some("run-other"))
        .expect_err("no other run follows run-flow's anchor");
    assert!(
        err.to_string().contains("opened by agent run run-flow"),
        "{err}"
    );

    terminals
        .close_agent_terminal(&mine.id)
        .expect("close agent terminal");
}
