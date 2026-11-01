use crate::mcp::catalog::{McpHost, SessionSummary, SpaceSummary, TerminalAngle, TerminalRead};
use crate::terminals::Terminals;
use crate::{SessionsEngine, WorkspaceHost};
use komet_proto::{TerminalInfo, TerminalRunResult};
use std::time::Duration;

/// Size of a terminal `terminal_open`/`terminal_run` creates for an agent: the
/// plain text default, matching what the UI uses when a caller omits a size.
const AGENT_TERMINAL_COLS: u16 = 80;
const AGENT_TERMINAL_ROWS: u16 = 24;
/// How long a marker-free terminal must stay quiet before `terminal_run`'s
/// fallback closes the stretch as `settled` (exit code then stays unknown —
/// never guessed).
const RUN_SILENCE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct EngineMcpHost {
    sessions: SessionsEngine,
    workspace: WorkspaceHost,
    /// The run this host serves. One host (and one catalog, one endpoint, one
    /// token) is built PER RUN (`SessionsEngine::start_internal_mcp`), and the
    /// endpoint has already matched its bearer token against this run id
    /// (`mcp::server::AuthState`) before any tool executes — so this is the
    /// verified caller identity the terminal gate bounds agent-opened
    /// terminals by, not a value an agent supplies.
    run_id: String,
}

impl EngineMcpHost {
    pub fn new(sessions: SessionsEngine, workspace: WorkspaceHost, run_id: &str) -> Self {
        Self {
            sessions,
            workspace,
            run_id: run_id.to_owned(),
        }
    }

    fn session_summary(&self, chat_id: &str) -> Option<SessionSummary> {
        let chat = self.workspace.chat(chat_id).ok()??;
        let status = self
            .sessions
            .session_status(chat_id)
            .map(|session| format!("{:?}", session.status).to_lowercase())
            .unwrap_or_else(|| "idle".to_owned());
        Some(SessionSummary {
            id: chat.id,
            title: chat.title.unwrap_or_else(|| "Untitled session".to_owned()),
            status,
        })
    }

    /// The engine's terminal registry, or the loud message an agent gets
    /// instead of an empty result (a silent nothing is not an answer).
    fn terminals(&self) -> Result<Terminals, String> {
        self.sessions
            .terminals()
            .ok_or_else(|| String::from("No terminals are wired on this engine"))
    }
}

#[async_trait::async_trait]
impl McpHost for EngineMcpHost {
    fn list_sessions(&self) -> Vec<SessionSummary> {
        self.workspace
            .read_chats()
            .unwrap_or_default()
            .into_iter()
            .map(|chat| {
                let status = self
                    .sessions
                    .session_status(&chat.id)
                    .map(|session| format!("{:?}", session.status).to_lowercase())
                    .unwrap_or_else(|| "idle".to_owned());
                SessionSummary {
                    id: chat.id,
                    title: chat.title.unwrap_or_else(|| "Untitled session".to_owned()),
                    status,
                }
            })
            .collect()
    }

    fn session_status(&self, id: &str) -> Option<SessionSummary> {
        self.session_summary(id)
    }

    fn list_spaces(&self) -> Vec<SpaceSummary> {
        self.workspace
            .read_spaces()
            .unwrap_or_default()
            .into_iter()
            .map(|space| SpaceSummary {
                id: space.id,
                path: space.path,
                name: space.name,
            })
            .collect()
    }

    /// Only what THIS run's gate passes: terminals a human granted, plus the
    /// ones this very run opened (bounded birth grant). An agent must not
    /// even learn the ids, cwds or titles of another run's agent-opened
    /// terminal or of the human's hidden ones (their scrollback holds
    /// tokens) — the filter reuses `Terminals::agent_gate`, the same
    /// predicate as the reads, so listing cannot leak what reading refuses.
    fn list_terminals_for_agents(&self) -> Vec<TerminalInfo> {
        let Ok(terminals) = self.terminals() else {
            return Vec::new();
        };
        terminals.list_terminals_for_reader(Some(&self.run_id))
    }

    /// The reads delegate to the GATED engine variants (`agent_*_for_run`)
    /// carrying THIS host's run id: the gate re-checks the human's grant or
    /// the run-bounded birth grant, then tracking, inside `crate::terminals` —
    /// the single enforcement point, so a catalog bug could not skip it. The
    /// refusal (or "not tracked") message travels to the agent verbatim; an
    /// invisible terminal never becomes an empty string here.
    fn read_terminal_for_agents(
        &self,
        terminal_id: &str,
        angle: &TerminalAngle,
    ) -> Result<TerminalRead, String> {
        let terminals = self.terminals()?;
        let reader = Some(self.run_id.as_str());
        match angle {
            TerminalAngle::Screen => terminals
                .agent_screen_text_for_run(terminal_id, reader)
                .map(TerminalRead::Screen),
            TerminalAngle::Tail { lines } => terminals
                .agent_tail_text_for_run(terminal_id, *lines, reader)
                .map(TerminalRead::Tail),
            TerminalAngle::Delta { since_seq } => terminals
                .agent_text_delta_for_run(terminal_id, *since_seq, reader)
                .map(TerminalRead::Delta),
            // The honest slice: `agent_last_command_for_run` re-checks the
            // read gate, then tracking, then refuses loudly when the shell
            // never emitted OSC 133 markers. Its message reaches the agent
            // verbatim.
            TerminalAngle::LastCommand => terminals
                .agent_last_command_for_run(terminal_id, reader)
                .map(TerminalRead::Command),
        }
        .map_err(|error| error.to_string())
    }

    // ---- write surface ---------------------------------------------------
    //
    // No read gate anywhere below on purpose: the door in front of these is
    // the MCP server's per-call human approval (`readonly: false` ⇒ `Ask` ⇒
    // nothing runs on a denial). Inside the engine the writes still pass
    // their own honest limits (`close_agent_terminal` refuses a human's
    // shell; `agent_run` switches tracking on so the run is observable;
    // `open_agent_terminal` stamps THIS run as the owner, which is what
    // bounds the born-readable grant to the run that opened the terminal).

    fn open_terminal_for_agent(&self, cwd: Option<String>) -> Result<TerminalInfo, String> {
        let terminals = self.terminals()?;
        let session = terminals
            .open_agent_terminal(
                cwd.as_deref(),
                AGENT_TERMINAL_COLS,
                AGENT_TERMINAL_ROWS,
                Some(&self.run_id),
            )
            .map_err(|error| error.to_string())?;
        terminals
            .list_terminals()
            .into_iter()
            .find(|terminal| terminal.terminal_id == session.id)
            .ok_or_else(|| format!("terminal {} opened but is not listed", session.id))
    }

    fn close_terminal_for_agent(&self, terminal_id: &str) -> Result<(), String> {
        self.terminals()?
            .close_agent_terminal(terminal_id)
            .map_err(|error| error.to_string())
    }

    fn send_input_to_terminal(&self, terminal_id: &str, bytes: &[u8]) -> Result<(), String> {
        self.terminals()?
            .write_raw(terminal_id, bytes)
            .map_err(|error| error.to_string())
    }

    async fn run_in_terminal(
        &self,
        terminal_id: &str,
        command: &str,
        wait_ms: u64,
    ) -> Result<TerminalRunResult, String> {
        self.terminals()?
            .agent_run(
                terminal_id,
                command,
                Duration::from_millis(wait_ms),
                RUN_SILENCE,
            )
            .await
            .map_err(|error| error.to_string())
    }
}
