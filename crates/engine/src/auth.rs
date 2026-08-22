use std::path::PathBuf;
use tokio::sync::watch;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMembership {
    pub id: String,
    pub organization_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    NeedsOrganization { user: AuthUser },
    SignedIn { user: AuthUser, org_id: Option<String> },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, AuthState::SignedIn { .. })
    }
    pub fn org_id(&self) -> Option<&str> {
        match self {
            AuthState::SignedIn { org_id, .. } => org_id.as_deref(),
            _ => None,
        }
    }
    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }
    pub fn to_proto(&self) -> komet_proto::AuthState {
        let profile = |user: &AuthUser| komet_proto::UserProfile {
            id: user.id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        };
        match self {
            AuthState::SignedOut => komet_proto::AuthState::SignedOut,
            AuthState::NeedsOrganization { user } => komet_proto::AuthState::NeedsOrganization { user: profile(user) },
            AuthState::SignedIn { user, org_id } => komet_proto::AuthState::SignedIn { user: profile(user), org_id: org_id.clone() },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub edge_url: String,
    pub data_dir: PathBuf,
    pub sync_token: Option<String>,
    pub dev_user_id: String,
}

impl AuthConfig {
    pub fn new(edge_url: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            edge_url: edge_url.into(),
            data_dir: data_dir.into(),
            sync_token: std::env::var("KOMET_SYNC_TOKEN").ok().filter(|s| !s.trim().is_empty()),
            dev_user_id: "local".into(),
        }
    }
}

#[derive(Clone)]
pub struct Auth {
    token: Option<String>,
    user_id: String,
    state_tx: watch::Sender<AuthState>,
    token_tx: watch::Sender<u64>,
}

impl Auth {
    pub fn new(config: AuthConfig) -> Self {
        let token = config.sync_token.clone().or_else(|| std::env::var("KOMET_SYNC_TOKEN").ok().filter(|s| !s.trim().is_empty()));
        let user_id = if config.dev_user_id.trim().is_empty() { "local".to_string() } else { config.dev_user_id.clone() };
        let user = AuthUser { id: user_id.clone(), email: user_id.clone(), name: None };
        let state = AuthState::SignedIn { user, org_id: None };
        let (state_tx, _) = watch::channel(state);
        let (token_tx, _) = watch::channel(0);
        Self { token, user_id, state_tx, token_tx }
    }

    pub async fn detect(config: AuthConfig) -> Self {
        Self::new(config)
    }

    pub fn workos_enabled(&self) -> bool { false }
    pub fn loaded_workos_session(&self) -> bool { false }
    pub fn watch_state(&self) -> watch::Receiver<AuthState> { self.state_tx.subscribe() }
    pub fn state(&self) -> AuthState { self.state_tx.borrow().clone() }
    pub fn user_id(&self) -> Option<String> { Some(self.user_id.clone()) }
    pub async fn access_token(&self) -> Option<String> { self.token.clone() }
    pub fn spawn_refresh_loop(&self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async {})
    }
    pub async fn start_sign_in(&self) -> Result<String, crate::EngineError> { Ok(String::new()) }
    pub fn start_headless_sign_in(&self) -> String { String::new() }
    pub async fn complete_sign_in(&self, _pasted: &str) -> Result<(), crate::EngineError> { Ok(()) }
    pub fn sign_out(&self) {}
    pub async fn list_orgs(&self) -> Result<Vec<OrgMembership>, crate::EngineError> { Ok(vec![]) }
    pub async fn create_org(&self, _name: &str) -> Result<(), crate::EngineError> { Ok(()) }
    pub async fn select_org(&self, _organization_id: &str) -> Result<(), crate::EngineError> { Ok(()) }
}

#[async_trait::async_trait]
impl komet_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> { self.token.clone() }
    fn subscribe(&self) -> Option<watch::Receiver<u64>> { Some(self.token_tx.subscribe()) }
}
