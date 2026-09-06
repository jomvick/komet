use std::collections::HashMap;

use super::config::{McpServerConfig, McpTransport};
use super::registry::McpRegistry;
use super::secrets::McpSecretStore;

#[test]
fn registry_persists_and_masks_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec!["mcp-github".into()],
        url: None,
        headers: Default::default(),
        env: [("GH_TOKEN".into(), "s3cr3t-xyz-999".into())].into(),
        always_load: false,
    })
    .unwrap();
    let list = reg.list_public();
    assert_eq!(list[0].id, "gh");
    // masqué: public view must not expose secret values, only has_secrets flag
    assert!(list[0].has_secrets);
    let debug = format!("{:?}", list[0]);
    assert!(!debug.contains("s3cr3t-xyz-999"));
    // headers/env values must not be exposed via public view debug
    // PublicMcpServerConfig has no headers/env fields — ensure no leak via serde
    let json = serde_json::to_string(&list[0]).unwrap();
    assert!(!json.contains("s3cr3t-xyz-999"));
    let reg2 = McpRegistry::load(dir.path()).unwrap();
    assert_eq!(reg2.list_public().len(), 1);
    assert_eq!(reg2.list_public()[0].id, "gh");
}

#[test]
fn registry_resolve_filters_enabled_and_resolves_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    // enabled server
    reg.save(&McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![],
        url: None,
        headers: HashMap::from([("Authorization".into(), "placeholder".into())]),
        env: HashMap::from([("GH_TOKEN".into(), "placeholder".into())]),
        always_load: false,
    })
    .unwrap();
    // disabled server
    reg.save(&McpServerConfig {
        id: "disabled".into(),
        name: "Disabled".into(),
        enabled: false,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    })
    .unwrap();
    // store secrets
    let secrets_path = dir.path().join("mcp-secrets.json");
    let store = McpSecretStore::new(&secrets_path);
    store.set_secret("gh", "Authorization", "Bearer resolved-secret").unwrap();
    store.set_secret("gh", "GH_TOKEN", "resolved-token").unwrap();

    let resolved = reg.resolve(&["gh".into(), "disabled".into(), "missing".into()], &store);
    // only enabled gh should be returned
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].config.id, "gh");
    assert_eq!(resolved[0].resolved_headers.get("Authorization").unwrap(), "Bearer resolved-secret");
    assert_eq!(resolved[0].resolved_env.get("GH_TOKEN").unwrap(), "resolved-token");
    // ensure placeholder not leaked, resolved values correct
    assert!(!format!("{:?}", resolved[0].config).contains("resolved-secret"));
}

#[test]
fn registry_save_validates_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    // invalid config: stdio without command
    let err = reg
        .save(&McpServerConfig {
            id: "bad".into(),
            name: "Bad".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: vec![],
            url: None,
            headers: Default::default(),
            env: Default::default(),
            always_load: false,
        })
        .unwrap_err();
    assert!(err.to_string().contains("command"));
    assert!(reg.list_public().is_empty());

    // valid save
    reg.save(&McpServerConfig {
        id: "ok".into(),
        name: "OK".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("cmd".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    })
    .unwrap();
    assert_eq!(reg.list_public().len(), 1);
}

#[test]
fn registry_add_update_delete_set_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    // add via save
    reg.save(&McpServerConfig {
        id: "a".into(),
        name: "A".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("cmd".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: false,
    })
    .unwrap();
    assert_eq!(reg.list_public().len(), 1);
    // update
    reg.save(&McpServerConfig {
        id: "a".into(),
        name: "A Updated".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("cmd2".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load: true,
    })
    .unwrap();
    assert_eq!(reg.list_public()[0].name, "A Updated");
    // set_enabled
    reg.set_enabled("a", false).unwrap();
    assert!(!reg.list_public()[0].enabled);
    // resolve should filter disabled
    let store = McpSecretStore::new(dir.path().join("mcp-secrets.json"));
    let resolved = reg.resolve(&["a".into()], &store);
    assert!(resolved.is_empty());
    // delete
    reg.delete("a").unwrap();
    assert!(reg.list_public().is_empty());
    // reload still empty
    let reg2 = McpRegistry::load(dir.path()).unwrap();
    assert!(reg2.list_public().is_empty());
}

#[test]
fn registry_list_public_masks_and_logs() {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&McpServerConfig {
        id: "s".into(),
        name: "SecretServer".into(),
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        url: Some("https://example.com/mcp".into()),
        headers: HashMap::from([("X-Token".into(), "super-secret".into())]),
        env: HashMap::from([("TOKEN".into(), "super-secret".into())]),
        always_load: false,
    })
    .unwrap();
    let list = reg.list_public();
    assert_eq!(list.len(), 1);
    assert!(list[0].has_secrets);
    // public view serialized should not contain secret
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains("super-secret"));
    // internal config debug also masked
    let resolved_store = McpSecretStore::new(dir.path().join("mcp-secrets.json"));
    let resolved = reg.resolve(&["s".into()], &resolved_store);
    // resolved headers/env will be empty because store empty, but config still masked
    assert_eq!(resolved.len(), 1);
    let cfg_debug = format!("{:?}", resolved[0].config);
    assert!(!cfg_debug.contains("super-secret"));
}
