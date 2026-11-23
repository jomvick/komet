use komet_proto::SyncStatus;
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub sync_token: Option<String>,
    pub dev_user_id: String,
}

impl AuthConfig {
    pub fn new() -> Self {
        Self {
            sync_token: std::env::var("KOMET_SYNC_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            dev_user_id: "local".into(),
        }
    }
}

#[derive(Clone)]
pub struct Auth {
    token: Option<String>,
    user_id: String,
    status_tx: watch::Sender<SyncStatus>,
}

impl Auth {
    pub fn new(config: AuthConfig) -> Self {
        let token = match config.sync_token {
            Some(token) => (!token.trim().is_empty()).then_some(token),
            None => std::env::var("KOMET_SYNC_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
        let user_id = if config.dev_user_id.trim().is_empty() {
            "local".to_string()
        } else {
            config.dev_user_id
        };
        let (status_tx, _) = watch::channel(SyncStatus {
            token_configured: token.is_some(),
        });
        Self {
            token,
            user_id,
            status_tx,
        }
    }

    pub fn watch_status(&self) -> watch::Receiver<SyncStatus> {
        self.status_tx.subscribe()
    }

    pub fn status(&self) -> SyncStatus {
        self.status_tx.borrow().clone()
    }

    pub fn user_id(&self) -> String {
        self.user_id.clone()
    }

    pub async fn access_token(&self) -> Option<String> {
        self.token.clone()
    }
}

#[async_trait::async_trait]
impl komet_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        self.token.clone()
    }

    fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        None
    }
}
