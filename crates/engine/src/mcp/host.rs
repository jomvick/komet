use crate::mcp::catalog::{McpHost, SessionSummary, SpaceSummary};
use crate::{SessionsEngine, WorkspaceHost};

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
}

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
}
