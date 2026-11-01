//! Terminal read transport: the `ListTerminals` / `ReadTerminal*` RPC surface
//! over the memory transport, and the engine-level reads behind the MCP
//! `terminal_list` / `terminal_read` tools. Every terminal is listable and
//! readable (no grant, no opt-in); nothing here lets an agent write.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use komet_engine::mcp::catalog::McpCatalog;
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

/// Poll the screen read until the marker shows (or blow up loudly — a read
/// that never answers is exactly the silent no-op this forbids).
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
            .expect("screen read");
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
async fn rpc_terminal_reads_need_no_grant() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Pin a bare shell and keep Repos' worktree root inside the tempdir.
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
    assert!(row.get("agentVisible").is_none(), "no visibility grant exists");
    assert_eq!(row["tracked"], true, "every terminal is mirrored");
    assert_eq!(row["cwd"], serde_json::json!(session.cwd));
    assert_eq!(row["shell"], "sh");
    assert_eq!(row["cols"], 80);
    assert_eq!(row["rows"], 24);

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
        .expect("tail read");
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
        .expect("delta read");
    assert_eq!(delta.from_seq, screen.seq);
    assert!(delta.through_seq >= screen.seq);

    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": id }),
        )
        .await
        .expect("CloseTerminal");
}

/// Agents can read ANY terminal: two freshly opened terminals are both
/// listed and readable with no grant, and each keeps its own text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_read_every_terminal_without_a_grant() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let terminals = Terminals::new();
    let cwd = tmp.path().to_string_lossy().to_string();
    let first = terminals
        .open_with_shell(&cwd, 80, 24, Some("/bin/sh"))
        .expect("open first");
    let second = terminals
        .open_with_shell(&cwd, 80, 24, Some("/bin/sh"))
        .expect("open second");

    for (session, word) in [(&first, "alph4 one"), (&second, "bet4 two")] {
        terminals
            .write(&session.id, &BASE64.encode(format!("echo {word}\n")))
            .expect("write");
    }
    for (session, word) in [(&first, "alph4 one"), (&second, "bet4 two")] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let screen = terminals.screen_text(&session.id).expect("ungated read");
            if screen.lines.iter().any(|line| line.contains(word)) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{word} never showed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let listed = terminals.list_terminals();
    for session in [&first, &second] {
        assert!(
            listed.iter().any(|info| info.terminal_id == session.id),
            "{} must be listed",
            session.id
        );
    }
    // A shell without OSC 133 markers refuses `last_command` loudly.
    let err = terminals
        .last_command(&first.id)
        .expect_err("plain sh emits no command markers");
    assert!(err.to_string().contains("no command markers"), "{err}");

    terminals.shutdown();
}

/// The agent-facing catalog only reads: no tool opens, closes, types into or
/// runs a command in a terminal.
#[test]
fn the_agent_catalog_has_no_terminal_write_tools() {
    struct NoHost;
    #[async_trait::async_trait]
    impl komet_engine::mcp::catalog::McpHost for NoHost {
        fn list_sessions(&self) -> Vec<komet_engine::mcp::catalog::SessionSummary> {
            vec![]
        }
        fn session_status(
            &self,
            _id: &str,
        ) -> Option<komet_engine::mcp::catalog::SessionSummary> {
            None
        }
        fn list_spaces(&self) -> Vec<komet_engine::mcp::catalog::SpaceSummary> {
            vec![]
        }
        fn list_terminals_for_agents(&self) -> Vec<komet_proto::TerminalInfo> {
            vec![]
        }
        fn read_terminal_for_agents(
            &self,
            _terminal_id: &str,
            _angle: &komet_engine::mcp::catalog::TerminalAngle,
        ) -> Result<komet_engine::mcp::catalog::TerminalRead, String> {
            Err("unused".into())
        }
    }
    let names: Vec<String> = McpCatalog::komet_default(Arc::new(NoHost))
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| name.starts_with("terminal_"))
        .collect();
    assert_eq!(names, vec!["terminal_list", "terminal_read"]);
}
