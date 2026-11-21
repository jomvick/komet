//! Native MCP servers Claude Code would load on its own, collected read-only
//! so a komet launch can reproduce them under `--strict-mcp-config`.
//!
//! Merge order (later entries override earlier ones on name collision):
//! 1. installed plugins' `.mcp.json` (`~/.claude/plugins/installed_plugins.json`
//!    → each `installPath/.mcp.json`);
//! 2. user scope `~/.claude.json` → `mcpServers`;
//! 3. project-scoped user entry `~/.claude.json` → `projects.<cwd>.mcpServers`;
//! 4. project scope `<cwd>/.mcp.json` → `mcpServers` (repo file wins).
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

    for path in plugin_mcp_json_paths(&home) {
        merge_mcp_servers(&path, &mut servers);
    }

    let claude_json = home.join(".claude.json");
    merge_mcp_servers(&claude_json, &mut servers);
    merge_project_scoped(&claude_json, cwd, &mut servers);

    if !cwd.is_empty() {
        merge_mcp_servers(&Path::new(cwd).join(".mcp.json"), &mut servers);
    }

    servers
}

fn plugin_mcp_json_paths(home: &Path) -> Vec<PathBuf> {
    let manifest = home.join(".claude/plugins/installed_plugins.json");
    let Ok(contents) = std::fs::read_to_string(&manifest) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let Some(plugins) = config.get("plugins").and_then(Value::as_object) else {
        return paths;
    };
    for installs in plugins.values() {
        let Some(list) = installs.as_array() else {
            continue;
        };
        for install in list {
            let Some(path) = install.get("installPath").and_then(Value::as_str) else {
                continue;
            };
            let candidate = Path::new(path).join(".mcp.json");
            if candidate.is_file() {
                paths.push(candidate);
            }
        }
    }
    paths
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
        let home = dir.path();
        // Redirect HOME for this test process — collect() reads platform::home_dir().
        // SAFETY-free approach: platform::home_dir reads env each call; tests run
        // multi-threaded so we avoid mutating env here and instead test the
        // merge helpers directly.
        let mut servers = BTreeMap::new();
        let plugin = home.join("plugin/.mcp.json");
        write(
            &plugin,
            r#"{"mcpServers":{"shared":{"command":"plugin"},"plug":{"command":"p"}}}"#,
        );
        let user = home.join(".claude.json");
        write(
            &user,
            r#"{"mcpServers":{"shared":{"command":"user"},"user-srv":{"command":"u"}},"projects":{"/repo":{"mcpServers":{"proj-scoped":{"command":"ps"}}}}}"#,
        );
        let project = home.join("repo/.mcp.json");
        write(
            &project,
            r#"{"mcpServers":{"shared":{"command":"project"},"proj":{"command":"pr"}}}"#,
        );

        merge_mcp_servers(&plugin, &mut servers);
        merge_mcp_servers(&user, &mut servers);
        merge_project_scoped(&user, "/repo", &mut servers);
        merge_mcp_servers(&project, &mut servers);

        // Project wins the "shared" name; every source contributes.
        assert_eq!(servers["shared"]["command"], "project");
        assert!(servers.contains_key("plug"));
        assert!(servers.contains_key("user-srv"));
        assert!(servers.contains_key("proj-scoped"));
        assert!(servers.contains_key("proj"));
        assert_eq!(servers.len(), 5);
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
    fn plugin_paths_come_from_installed_plugins_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            r#"{"version":2,"plugins":{"ecc@ecc":[{"installPath":"/plugins/ecc/2.2.3"}]}}"#,
        );
        // installPath is absolute in real manifests; simulate by writing into
        // the tempdir and rewriting the manifest to point there.
        let install = home.join("plugins/ecc/2.2.3");
        write(
            &install.join(".mcp.json"),
            r#"{"mcpServers":{"x":{"command":"y"}}}"#,
        );
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &format!(
                r#"{{"version":2,"plugins":{{"ecc@ecc":[{{"installPath":"{}"}}]}}}}"#,
                install.display()
            ),
        );
        let paths = plugin_mcp_json_paths(home);
        assert_eq!(paths, vec![install.join(".mcp.json")]);
    }
}
