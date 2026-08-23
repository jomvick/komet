//! Auth service tests: komet is local-first — the WorkOS flows were removed
//! with the edge server, so these cover the stubbed surface that remains
//! (dev sign-in, no-op headless flow, token passthrough).

use komet_engine::{Auth, AuthConfig, AuthState};

#[tokio::test]
async fn dev_mode_is_signed_in_with_configured_bearer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AuthConfig::new("http://127.0.0.1:1", dir.path());
    config.dev_user_id = "test-dev".into();
    let auth = Auth::new(config);
    assert!(!auth.workos_enabled());
    assert!(!auth.loaded_workos_session());
    assert!(matches!(auth.state(), AuthState::SignedIn { user, .. } if user.id == "test-dev"));
    // No sync_token configured: no bearer (local-first stays offline).
    assert_eq!(auth.access_token().await.as_deref(), None);
    // Dev sign-in mirrors the TS service: a no-op URL, CompleteSignIn accepted.
    assert_eq!(auth.start_sign_in().await.expect("dev sign-in"), "");
    auth.complete_sign_in("whatever")
        .await
        .expect("dev complete is a no-op");
}

#[tokio::test]
async fn local_first_defaults_sign_in_without_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(AuthConfig::new("http://127.0.0.1:1", dir.path()));
    assert!(matches!(auth.state(), AuthState::SignedIn { user, .. } if user.id == "local"));
    assert_eq!(auth.user_id().as_deref(), Some("local"));
    // Headless flow is a no-op string; completing it is a no-op success.
    assert_eq!(auth.start_headless_sign_in(), "");
    auth.complete_sign_in("any.code")
        .await
        .expect("stubbed flow accepts anything");
    auth.sign_out();
}

#[tokio::test]
async fn sync_token_env_becomes_the_bearer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AuthConfig::new("http://127.0.0.1:1", dir.path());
    config.sync_token = Some("tok-123".into());
    let auth = Auth::new(config);
    assert_eq!(auth.access_token().await.as_deref(), Some("tok-123"));
}
