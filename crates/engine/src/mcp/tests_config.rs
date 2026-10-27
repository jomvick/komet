use std::collections::HashMap;

use super::config::{McpServerConfig, McpTransport};
use super::secrets::McpSecretStore;

#[test]
fn rejects_stdio_without_command() {
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: None,
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn http_requires_url() {
    let cfg = McpServerConfig {
        id: "n".into(),
        name: "N".into(),
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn sse_requires_url() {
    let cfg = McpServerConfig {
        id: "s".into(),
        name: "S".into(),
        enabled: true,
        transport: McpTransport::Sse,
        command: None,
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn rejects_empty_id_or_name() {
    let cfg = McpServerConfig {
        id: "".into(),
        name: "N".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("cmd".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(cfg.validate().is_err());
    let cfg2 = McpServerConfig {
        id: "id".into(),
        name: "".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("cmd".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(cfg2.validate().is_err());
}

#[test]
fn accepts_valid_stdio_and_http() {
    let stdio = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec!["mcp-github".into()],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(stdio.validate().is_ok());
    let http = McpServerConfig {
        id: "n".into(),
        name: "N".into(),
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        url: Some("https://example.com/mcp".into()),
        headers: Default::default(),
        env: Default::default(),
        always_load: true,
    };
    assert!(http.validate().is_ok());
}

#[test]
fn public_view_masks_secrets() {
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![],
        url: None,
        headers: HashMap::from([("Authorization".into(), "Bearer secret".into())]),
        env: HashMap::from([("GH_TOKEN".into(), "secret".into())]),
        always_load: false,
    };
    let pub_cfg = cfg.public_view();
    assert_eq!(pub_cfg.id, "gh");
    assert!(pub_cfg.has_secrets);
    // Public view must not contain secret values in debug repr (field name has_secrets is ok)
    let debug = format!("{pub_cfg:?}");
    assert!(!debug.contains("Bearer"));
}

#[test]
fn public_view_has_secrets_false_when_empty() {
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    };
    assert!(!cfg.public_view().has_secrets);
}

#[test]
fn debug_masks_header_and_env_values() {
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        url: Some("https://example.com".into()),
        headers: HashMap::from([("X-Token".into(), "super-secret".into())]),
        env: HashMap::from([("TOKEN".into(), "super-secret".into())]),
        always_load: false,
    };
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("super-secret"));
    assert!(debug.contains("masked") || debug.contains("keys"));
}

#[test]
fn serde_roundtrip_preserves_fields() {
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Sse,
        command: None,
        args: vec!["a".into()],
        url: Some("https://example.com/sse".into()),
        headers: HashMap::from([("H".into(), "v".into())]),
        env: HashMap::new(),
        always_load: true,
    };
    let json = serde_json::to_string(&cfg).unwrap();
    let back: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, back);
}

#[test]
fn secret_store_resolve_and_set_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-secrets.json");
    let store = McpSecretStore::new(&path);
    assert!(store.resolve("gh").is_empty());
    store.set_secret("gh", "GH_TOKEN", "secret123").unwrap();
    let resolved = store.resolve("gh");
    assert_eq!(resolved.get("GH_TOKEN").unwrap(), "secret123");
    // Reload from file
    let store2 = McpSecretStore::new(&path);
    assert_eq!(store2.resolve("gh").get("GH_TOKEN").unwrap(), "secret123");
    // Debug must not leak
    let debug = format!("{store2:?}");
    assert!(!debug.contains("secret123"));
    assert!(debug.contains("McpSecretStore"));
}

#[test]
fn secret_store_file_has_600_perms_on_unix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-secrets.json");
    let store = McpSecretStore::new(&path);
    store.set_secret("gh", "k", "v").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}

#[test]
fn secret_store_partitioned_headers_and_env() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-secrets.json");
    let store = McpSecretStore::new(&path);
    store.set_secret("gh", "Authorization", "Bearer x").unwrap();
    store.set_secret("gh", "GH_TOKEN", "tok123").unwrap();
    // Unfiltered resolve returns both
    assert_eq!(store.resolve("gh").len(), 2);
    // Filtered views distinguish headers vs env via config key sets
    let cfg_headers = HashMap::from([("Authorization".into(), "dummy".into())]);
    let cfg_env = HashMap::from([("GH_TOKEN".into(), "dummy".into())]);
    let headers = store.get_resolved_headers_for("gh", &cfg_headers);
    let env = store.get_resolved_env_for("gh", &cfg_env);
    assert_eq!(headers.get("Authorization").unwrap(), "Bearer x");
    assert!(headers.get("GH_TOKEN").is_none());
    assert_eq!(env.get("GH_TOKEN").unwrap(), "tok123");
    assert!(env.get("Authorization").is_none());
    // Aliases still return merged (documented)
    assert_eq!(store.get_resolved_headers("gh").len(), 2);
    assert_eq!(store.get_resolved_env("gh").len(), 2);
}

#[test]
fn ephemeral_uses_unique_path() {
    let a = McpSecretStore::ephemeral();
    let b = McpSecretStore::ephemeral();
    assert_ne!(a.path(), b.path());
    assert!(
        a.path()
            .to_string_lossy()
            .contains("komet-mcp-secrets-ephemeral")
    );
}
