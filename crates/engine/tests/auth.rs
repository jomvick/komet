//! Shared-token configuration tests for self-hosted synchronization.

use komet_engine::{Auth, AuthConfig};

#[tokio::test]
async fn configured_development_identity_has_no_sync_bearer_by_default() {
    let mut config = AuthConfig::new();
    config.dev_user_id = "test-dev".into();
    let auth = Auth::new(config);
    assert_eq!(auth.user_id(), "test-dev");
    assert_eq!(auth.access_token().await, None);
    assert!(!auth.status().token_configured);
}

#[tokio::test]
async fn missing_identity_uses_local_development_identity() {
    let auth = Auth::new(AuthConfig::new());
    assert_eq!(auth.user_id(), "local");
}

#[tokio::test]
async fn sync_token_env_becomes_the_bearer() {
    let mut config = AuthConfig::new();
    config.sync_token = Some("tok-123".into());
    let auth = Auth::new(config);
    assert_eq!(auth.access_token().await.as_deref(), Some("tok-123"));
    assert!(auth.status().token_configured);
}

#[tokio::test]
async fn blank_sync_token_does_not_enable_sync() {
    let mut config = AuthConfig::new();
    config.sync_token = Some("  \n".into());
    let auth = Auth::new(config);
    assert_eq!(auth.access_token().await, None);
    assert!(!auth.status().token_configured);
}
