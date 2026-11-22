//! Native MCP servers Claude Code would load on its own, collected read-only
//! so a komet launch can reproduce them under `--strict-mcp-config`.
//!
//! Merge order (later entries override earlier ones on name collision),
//! matching Claude Code's documented scope precedence (lowest → highest):
//! 1. enabled plugins' MCP servers — `<install>/.mcp.json` then the
//!    `.claude-plugin/plugin.json` `mcpServers` manifest key (manifest wins
//!    on a name collision, per plugin docs);
//! 2. user scope `~/.claude.json` → `mcpServers`;
//! 3. project-scoped user entry `~/.claude.json` → `projects.<cwd>.mcpServers`;
//! 4. project scope `<cwd>/.mcp.json`;
//! 5. local scope `<cwd>/.mcp.local.json` (gitignored per-machine overrides).
//!
//! A plugin only contributes when it is effectively enabled: the merged
//! `enabledPlugins` map across user → project → local settings wins per
//! plugin id, and an id absent everywhere falls back to the plugin manifest's
//! `defaultEnabled` (default `true`). Managed settings are not consulted —
//! their path is platform-specific and rarely deployed; noted limitation.
//!
//! Values are passed through verbatim — they are the agent's own config, not
//! komet data. Nothing here writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::platform::home_dir;

/// Merged native `mcpServers` map for a Claude run in `cwd`.
pub fn collect(cwd: &str) -> BTreeMap<String, Value> {
    let mut servers = BTreeMap::new();
    let Some(home) = home_dir() else {
        return servers;
    };

    let enabled_plugins = enabled_plugins_map(&home, cwd);
    for (plugin_id, install_root) in plugin_installs(&home, cwd) {
        if !plugin_enabled(&enabled_plugins, &install_root, &plugin_id) {
            continue;
        }
        // `.mcp.json` first: a same-named manifest server replaces it.
        merge_mcp_servers(&install_root.join(".mcp.json"), &mut servers);
        merge_plugin_manifest_mcp(&install_root, &mut servers);
    }

    let claude_json = home.join(".claude.json");
    merge_mcp_servers(&claude_json, &mut servers);
    merge_project_scoped(&claude_json, cwd, &mut servers);

    if !cwd.is_empty() {
        let project = Path::new(cwd);
        merge_mcp_servers(&project.join(".mcp.json"), &mut servers);
        merge_mcp_servers(&project.join(".mcp.local.json"), &mut servers);
    }

    servers
}

/// `(plugin_id, install_root)` pairs from the user and project plugin
/// manifests. When several installs of one id exist, the LAST entry wins —
/// manifests append on update.
fn plugin_installs(home: &Path, cwd: &str) -> Vec<(String, PathBuf)> {
    let mut manifests = vec![home.join(".claude/plugins/installed_plugins.json")];
    if !cwd.is_empty() {
        manifests.push(Path::new(cwd).join(".claude/plugins/installed_plugins.json"));
    }
    let mut by_id: BTreeMap<String, PathBuf> = BTreeMap::new();
    for manifest in manifests {
        let Ok(contents) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        let Some(plugins) = config.get("plugins").and_then(Value::as_object) else {
            continue;
        };
        for (id, installs) in plugins {
            let Some(list) = installs.as_array() else {
                continue;
            };
            for install in list {
                let Some(path) = install.get("installPath").and_then(Value::as_str) else {
                    continue;
                };
                by_id.insert(id.clone(), PathBuf::from(path));
            }
        }
    }
    by_id.into_iter().collect()
}

/// `enabledPlugins` merged across the settings scopes that apply to `cwd`,
/// lowest → highest precedence: user, project, local. Later scopes override
/// per plugin id; ids never mentioned stay absent.
fn enabled_plugins_map(home: &Path, cwd: &str) -> BTreeMap<String, bool> {
    let mut map = BTreeMap::new();
    let mut paths = vec![home.join(".claude/settings.json")];
    if !cwd.is_empty() {
        let project = Path::new(cwd).join(".claude");
        paths.push(project.join("settings.json"));
        paths.push(project.join("settings.local.json"));
    }
    for path in paths {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        let Some(enabled) = config.get("enabledPlugins").and_then(Value::as_object) else {
            continue;
        };
        for (id, value) in enabled {
            if let Some(b) = value.as_bool() {
                map.insert(id.clone(), b);
            }
        }
    }
    map
}

/// Effective enablement of one plugin install: explicit `enabledPlugins`
/// entry wins; absent everywhere falls back to the manifest's
/// `defaultEnabled` (Claude's documented default is `true`).
fn plugin_enabled(
    enabled_plugins: &BTreeMap<String, bool>,
    install_root: &Path,
    plugin_id: &str,
) -> bool {
    if let Some(enabled) = enabled_plugins.get(plugin_id) {
        return *enabled;
    }
    let manifest = install_root.join(".claude-plugin/plugin.json");
    let Ok(contents) = std::fs::read_to_string(&manifest) else {
        return true;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return true;
    };
    config
        .get("defaultEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Merge the `mcpServers` declared by a plugin's
/// `.claude-plugin/plugin.json`. The manifest key accepts an inline server
/// map, a path to a JSON file, or an array mixing both — resolved relative
/// to the plugin root. Manifest entries override `.mcp.json` names, so this
/// runs after it.
fn merge_plugin_manifest_mcp(install_root: &Path, servers: &mut BTreeMap<String, Value>) {
    let manifest = install_root.join(".claude-plugin/plugin.json");
    let Ok(contents) = std::fs::read_to_string(&manifest) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    let Some(mcp) = config.get("mcpServers") else {
        return;
    };
    let mut entries = BTreeMap::new();
    collect_manifest_mcp(mcp, install_root, &mut entries);
    servers.append(&mut entries);
}

/// Recursively collect server entries from a manifest `mcpServers` value:
/// object → name→config pairs; string → path to a JSON file (same shapes
/// recursively); array → each element.
fn collect_manifest_mcp(value: &Value, install_root: &Path, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::String(rel) => {
            let path = Path::new(rel);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                install_root.join(path)
            };
            let Ok(contents) = std::fs::read_to_string(&path) else {
                return;
            };
            let Ok(file) = serde_json::from_str::<Value>(&contents) else {
                return;
            };
            // Referenced files may be `.mcp.json`-shaped or a bare server map.
            let inner = file.get("mcpServers").unwrap_or(&file);
            collect_manifest_mcp(inner, install_root, out);
        }
        Value::Array(items) => {
            for item in items {
                collect_manifest_mcp(item, install_root, out);
            }
        }
        Value::Object(map) => {
            // A wrapper object (`{"mcpServers": {...}}`) unwraps once; anything
            // else is treated as the server map itself.
            if let Some(inner) = map.get("mcpServers")
                && map.len() == 1
            {
                collect_manifest_mcp(inner, install_root, out);
                return;
            }
            for (name, config) in map {
                if config.is_object() {
                    out.insert(name.clone(), config.clone());
                }
            }
        }
        _ => {}
    }
}

/// Read `<path>` as a Claude MCP config (`{"mcpServers": {...}}`) and merge
/// its entries into `servers`.
fn merge_mcp_servers(path: &Path, servers: &mut BTreeMap<String, Value>) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    let Some(mcp) = config.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in mcp {
        servers.insert(name.clone(), value.clone());
    }
}

/// `~/.claude.json` keeps per-project approvals under `projects.<abs path>`.
/// Only an exact cwd match is merged — guessing at path normalization would
/// risk enabling servers the user never approved for this directory.
fn merge_project_scoped(claude_json: &Path, cwd: &str, servers: &mut BTreeMap<String, Value>) {
    if cwd.is_empty() {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(claude_json) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    let Some(project) = config
        .get("projects")
        .and_then(Value::as_object)
        .and_then(|p| p.get(cwd))
    else {
        return;
    };
    let Some(mcp) = project.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in mcp {
        servers.insert(name.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn merges_sources_in_precedence_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut servers = BTreeMap::new();
        let plugin = dir.path().join("plugin/.mcp.json");
        write(
            &plugin,
            r#"{"mcpServers":{"shared":{"command":"plugin"},"plug":{"command":"p"}}}"#,
        );
        let user = dir.path().join(".claude.json");
        write(
            &user,
            r#"{"mcpServers":{"shared":{"command":"user"},"user-srv":{"command":"u"}},"projects":{"/repo":{"mcpServers":{"proj-scoped":{"command":"ps"}}}}}"#,
        );
        let project = dir.path().join("repo/.mcp.json");
        write(
            &project,
            r#"{"mcpServers":{"shared":{"command":"project"},"proj":{"command":"pr"}}}"#,
        );
        let local = dir.path().join("repo/.mcp.local.json");
        write(
            &local,
            r#"{"mcpServers":{"shared":{"command":"local"},"local-srv":{"command":"l"}}}"#,
        );

        merge_mcp_servers(&plugin, &mut servers);
        merge_mcp_servers(&user, &mut servers);
        merge_project_scoped(&user, "/repo", &mut servers);
        merge_mcp_servers(&project, &mut servers);
        merge_mcp_servers(&local, &mut servers);

        // Local scope wins the "shared" name; every source contributes.
        assert_eq!(servers["shared"]["command"], "local");
        assert!(servers.contains_key("plug"));
        assert!(servers.contains_key("user-srv"));
        assert!(servers.contains_key("proj-scoped"));
        assert!(servers.contains_key("proj"));
        assert!(servers.contains_key("local-srv"));
        assert_eq!(servers.len(), 6);
    }

    #[test]
    fn project_scoped_requires_exact_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join(".claude.json");
        write(
            &user,
            r#"{"projects":{"/repo":{"mcpServers":{"s":{"command":"c"}}}}}"#,
        );
        let mut servers = BTreeMap::new();
        merge_project_scoped(&user, "/repo/", &mut servers);
        assert!(servers.is_empty());
        merge_project_scoped(&user, "/repo", &mut servers);
        assert!(servers.contains_key("s"));
    }

    #[test]
    fn plugin_installs_and_manifest_mcp_merge() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let install = dir.path().join("plugins/ecc/2.2.3");
        write(
            &install.join(".mcp.json"),
            r#"{"mcpServers":{"dot-mcp":{"command":"a"},"shared":{"command":"file"}}}"#,
        );
        write(
            &install.join(".claude-plugin/plugin.json"),
            &serde_json::json!({
                "name": "ecc",
                "mcpServers": {
                    "manifest-inline": {"command": "b"},
                    "shared": {"command": "manifest"}
                }
            })
            .to_string(),
        );
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &serde_json::json!({
                "version": 2,
                "plugins": {"ecc@ecc": [{"installPath": install.to_string_lossy()}]}
            })
            .to_string(),
        );

        let installs = plugin_installs(&home, "");
        assert_eq!(installs, vec![("ecc@ecc".to_string(), install.clone())]);

        let mut servers = BTreeMap::new();
        merge_mcp_servers(&install.join(".mcp.json"), &mut servers);
        merge_plugin_manifest_mcp(&install, &mut servers);
        assert_eq!(servers["dot-mcp"]["command"], "a");
        assert_eq!(servers["manifest-inline"]["command"], "b");
        // Manifest server replaces the same-named .mcp.json entry.
        assert_eq!(servers["shared"]["command"], "manifest");
    }

    #[test]
    fn disabled_plugin_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let install = dir.path().join("plugins/ecc/2.2.3");
        write(
            &install.join(".mcp.json"),
            r#"{"mcpServers":{"x":{"command":"y"}}}"#,
        );
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &serde_json::json!({
                "version": 2,
                "plugins": {"ecc@ecc": [{"installPath": install}]}
            })
            .to_string(),
        );
        write(
            &home.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"ecc@ecc":false}}"#,
        );

        let enabled = enabled_plugins_map(&home, "");
        let installs = plugin_installs(&home, "");
        assert!(
            installs
                .iter()
                .all(|(_, root)| !plugin_enabled(&enabled, root, "ecc@ecc"))
        );
    }

    #[test]
    fn local_settings_override_project_for_enabled_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("repo");
        write(
            &home.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"p@m":true}}"#,
        );
        write(
            &cwd.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"p@m":false}}"#,
        );
        write(
            &cwd.join(".claude/settings.local.json"),
            r#"{"enabledPlugins":{"p@m":true}}"#,
        );
        let enabled = enabled_plugins_map(&home, cwd.to_str().unwrap());
        assert_eq!(enabled.get("p@m"), Some(&true));
    }
}
