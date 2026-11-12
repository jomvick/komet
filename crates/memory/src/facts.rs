use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Decision,
    Bug,
    Convention,
    State,
    Todo,
    FailedAttempt,
}

impl FactKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Bug => "bug",
            Self::Convention => "convention",
            Self::State => "state",
            Self::Todo => "todo",
            Self::FailedAttempt => "failed_attempt",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "decision" => Some(Self::Decision),
            "bug" => Some(Self::Bug),
            "convention" => Some(Self::Convention),
            "state" => Some(Self::State),
            "todo" => Some(Self::Todo),
            "failed_attempt" => Some(Self::FailedAttempt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactScope {
    Session,
    Project,
    Global,
}

impl FactScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
            Self::Global => "global",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            "project" => Some(Self::Project),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    AgentAssertion,
    Observed,
    Test,
    UserConfirmed,
}

impl FactSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AgentAssertion => "agent_assertion",
            Self::Observed => "observed",
            Self::Test => "test",
            Self::UserConfirmed => "user_confirmed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "agent_assertion" => Some(Self::AgentAssertion),
            "observed" => Some(Self::Observed),
            "test" => Some(Self::Test),
            "user_confirmed" => Some(Self::UserConfirmed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMemoryFact {
    pub project_id: String,
    pub chat_id: Option<String>,
    pub kind: FactKind,
    pub scope: FactScope,
    pub content: String,
    pub source: FactSource,
    pub source_agent: Option<String>,
    pub source_model: Option<String>,
    pub source_event_seq: Option<u64>,
    pub source_path: Option<String>,
    pub confidence: f64,
    pub user_confirmed: bool,
    pub valid_from_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFact {
    pub id: i64,
    pub project_id: String,
    pub chat_id: Option<String>,
    pub kind: FactKind,
    pub scope: FactScope,
    pub content: String,
    pub source: FactSource,
    pub source_agent: Option<String>,
    pub source_model: Option<String>,
    pub source_event_seq: Option<u64>,
    pub source_path: Option<String>,
    pub confidence: f64,
    pub user_confirmed: bool,
    pub valid_from_ms: i64,
    pub stale_at_ms: Option<i64>,
    pub replaced_by: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticMemoryMatch {
    pub fact: MemoryFact,
    pub distance: f32,
}

impl NewMemoryFact {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.trim().is_empty() {
            return Err("project_id must not be empty");
        }
        let content = self.content.trim();
        if content.is_empty() || content.len() > 8192 {
            return Err("fact content must be between 1 and 8192 bytes");
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err("confidence must be between 0 and 1");
        }
        if self
            .source_event_seq
            .is_some_and(|seq| seq > i64::MAX as u64)
        {
            return Err("source_event_seq is out of range");
        }
        match self.scope {
            FactScope::Session if self.chat_id.as_deref().is_none_or(str::is_empty) => {
                return Err("session facts require a chat_id");
            }
            FactScope::Global if !self.user_confirmed => {
                return Err("global facts require explicit user confirmation");
            }
            _ => {}
        }
        Ok(())
    }
}
