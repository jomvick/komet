use crate::mcp::catalog::{McpHost, SessionSummary, SpaceSummary, TerminalAngle, TerminalRead};
use crate::terminals::Terminals;
use crate::{SessionsEngine, WorkspaceHost};
use komet_proto::TerminalInfo;

#[derive(Clone)]
pub struct EngineMcpHost {
    sessions: SessionsEngine,
    workspace: WorkspaceHost,
}

impl EngineMcpHost {
    pub fn new(sessions: SessionsEngine, workspace: WorkspaceHost) -> Self {
        Self {
            sessions,
            workspace,
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

    fn list_terminals_for_agents(&self) -> Vec<TerminalInfo> {
        self.terminals()
            .map(|terminals| terminals.list_terminals())
            .unwrap_or_default()
    }

    fn read_terminal_for_agents(
        &self,
        terminal_id: &str,
        angle: &TerminalAngle,
    ) -> Result<TerminalRead, String> {
        let terminals = self.terminals()?;
        match angle {
            TerminalAngle::Screen => terminals.screen_text(terminal_id).map(TerminalRead::Screen),
            TerminalAngle::Tail { lines } => terminals
                .tail_text(terminal_id, *lines)
                .map(TerminalRead::Tail),
            TerminalAngle::Delta { since_seq } => terminals
                .text_delta(terminal_id, *since_seq)
                .map(TerminalRead::Delta),
            // Refuses loudly when the shell never emitted OSC 133 markers.
            TerminalAngle::LastCommand => terminals
                .last_command(terminal_id)
                .map(TerminalRead::Command),
        }
        .map_err(|error| error.to_string())
    }
}
