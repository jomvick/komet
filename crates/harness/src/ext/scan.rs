//! Read-only scanners: walk each agent's own config locations and produce the
//! shared `ExtItem` inventory. Nothing here writes — the only komet-owned
//! state is the `ExtOverlay`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use komet_proto::HarnessId;
use serde_json::Value;

use crate::ext::model::{ExtItem, ExtKind, ExtSource, Origin};

/// Home-relative skill directories per agent. Evidence-based: only locations
/// verified on a real install are listed; extend as agents are checked.
const AGENT_SKILL_DIRS: &[(HarnessId, &[&str])] = &[
    (HarnessId::ClaudeCode, &[".claude/skills"]),
    (HarnessId::Codex, &[".codex/skills"]),
    (HarnessId::Cursor, &[".cursor/skills"]),
    (
        HarnessId::Opencode,
        &[".config/opencode/skills", ".opencode/skills"],
    ),
    (HarnessId::Copilot, &[".copilot/skills"]),
    (HarnessId::Devin, &[".config/devin/skills"]),
];

/// `~/.agents/skills` is the cross-agent shared folder. Only agents with
/// observed consumption are listed — Copilot (komet links it into the
/// isolated COPILOT_HOME) and Devin (native skill root). Extend per evidence.
const SHARED_SKILL_DIR: &str = ".agents/skills";
const SHARED_SKILL_AGENTS: &[HarnessId] = &[HarnessId::Devin, HarnessId::Copilot];

/// Scan `home` (usually `platform::home_dir()`) for agent-native extensions.
/// Project-level configs (`.claude/` inside a repo, opencode.json up the cwd
/// chain) are intentionally out of scope: this inventory is the global one.
pub fn scan(home: &Path) -> Vec<ExtItem> {
    let mut items: BTreeMap<(ExtKind, String), ExtItem> = BTreeMap::new();
    let mut push = |kind: ExtKind, id: String, name: String, source: ExtSource| {
        let entry = items.entry((kind, id.clone())).or_insert_with(|| ExtItem {
            kind,
            id,
            name: name.clone(),
            origin: Origin::External,
            sources: Vec::new(),
        });
        if entry.name.is_empty() {
            entry.name = name;
        }
        if !entry.sources.contains(&source) {
            entry.sources.push(source);
        }
    };

    scan_skills(home, &mut push);
    scan_mcp(home, &mut push);
    scan_plugins(home, &mut push);

    let mut out: Vec<ExtItem> = items.into_values().collect();
    out.sort_by(|a, b| (a.kind.as_str(), &a.id).cmp(&(b.kind.as_str(), &b.id)));
    out
}

type Push<'a> = dyn FnMut(ExtKind, String, String, ExtSource) + 'a;

fn scan_skills(home: &Path, push: &mut Push<'_>) {
    let mut dirs: Vec<(HarnessId, PathBuf)> = Vec::new();
    for (agent, rels) in AGENT_SKILL_DIRS {
        for rel in *rels {
            dirs.push((*agent, home.join(rel)));
        }
    }
    for agent in SHARED_SKILL_AGENTS {
        dirs.push((*agent, home.join(SHARED_SKILL_DIR)));
    }

    for (agent, dir) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let name = skill_display_name(&skill_md).unwrap_or_else(|| id.clone());
            push(
                ExtKind::Skill,
                id,
                name,
                ExtSource {
                    agent,
                    path: skill_dir,
                    enabled: None,
                },
            );
        }
    }
}

/// First `name:` / `description:` from SKILL.md YAML frontmatter, if any.
fn skill_display_name(skill_md: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(skill_md).ok()?;
    let mut in_frontmatter = false;
    for line in contents.lines() {
        let line = line.trim();
        if line == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        for key in ["name:", "description:"] {
            if let Some(rest) = line.strip_prefix(key) {
                let value = rest.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn scan_mcp(home: &Path, push: &mut Push<'_>) {
    scan_claude_mcp(home, push);
    scan_cursor_mcp(home, push);
    scan_copilot_mcp(home, push);
    scan_codex_mcp(home, push);
    scan_opencode_mcp(home, push);
}

fn mcp_servers_object(config: &Value) -> Option<&serde_json::Map<String, Value>> {
    config.get("mcpServers")?.as_object()
}

fn push_mcp_entries(
    servers: &serde_json::Map<String, Value>,
    agent: HarnessId,
    path: &Path,
    push: &mut Push<'_>,
) {
    for (name, value) in servers {
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .or_else(|| value.get("disabled").and_then(Value::as_bool).map(|d| !d));
        let display = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string();
        push(
            ExtKind::McpServer,
            name.clone(),
            display,
            ExtSource {
                agent,
                path: path.to_path_buf(),
                enabled,
            },
        );
    }
}

/// `~/.claude.json` top-level `mcpServers` (user scope only — project-scoped
/// entries under `projects.*` are out of scope for the global inventory).
fn scan_claude_mcp(home: &Path, push: &mut Push<'_>) {
    let path = home.join(".claude.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    if let Some(servers) = mcp_servers_object(&config) {
        push_mcp_entries(servers, HarnessId::ClaudeCode, &path, push);
    }
}

fn scan_cursor_mcp(home: &Path, push: &mut Push<'_>) {
    let path = home.join(".cursor/mcp.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    if let Some(servers) = mcp_servers_object(&config) {
        push_mcp_entries(servers, HarnessId::Cursor, &path, push);
    }
}

fn scan_copilot_mcp(home: &Path, push: &mut Push<'_>) {
    let path = home.join(".copilot/mcp-config.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    if let Some(servers) = mcp_servers_object(&config) {
        push_mcp_entries(servers, HarnessId::Copilot, &path, push);
    }
}

/// `~/.codex/config.toml` `[mcp_servers.<name>]` tables. Line-scanned rather
/// than TOML-parsed: we only need section names and an optional `enabled`
/// flag, and a serde `toml` dependency is not in the workspace yet.
fn scan_codex_mcp(home: &Path, push: &mut Push<'_>) {
    let path = home.join(".codex/config.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut current: Option<String> = None;
    let mut enabled: Option<bool> = None;
    let mut flush = |name: &mut Option<String>, enabled: &mut Option<bool>| {
        if let Some(n) = name.take() {
            push(
                ExtKind::McpServer,
                n.clone(),
                n,
                ExtSource {
                    agent: HarnessId::Codex,
                    path: path.clone(),
                    enabled: *enabled,
                },
            );
            *enabled = None;
        }
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut current, &mut enabled);
            current = parse_mcp_server_section(line);
            continue;
        }
        if let Some(name) = &current
            && let Some(value) = line
                .strip_prefix("enabled")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            let value = value.trim();
            enabled = Some(matches!(value, "true"));
            let _ = name;
        }
    }
    flush(&mut current, &mut enabled);
}

/// `[mcp_servers.foo]` / `[mcp_servers."foo bar"]` → `foo`. Other tables
/// (including bare `[mcp_servers]`) yield `None`.
fn parse_mcp_server_section(header: &str) -> Option<String> {
    let inner = header.trim().trim_start_matches('[').trim_end_matches(']');
    let rest = inner.trim().strip_prefix("mcp_servers.")?;
    let name = rest.trim().trim_end_matches('.');
    if name.is_empty() || name.contains('.') {
        return None;
    }
    let name = name.trim_matches('"').trim_matches('\'');
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// OpenCode global configs only: `OPENCODE_CONFIG`, XDG config dir,
/// `~/.opencode`, `~/.config/opencode`. Project-directory opencode.json files
/// are launch-context, not part of the global inventory.
fn scan_opencode_mcp(home: &Path, push: &mut Push<'_>) {
    let mut paths = Vec::new();
    if let Some(custom) = std::env::var_os("OPENCODE_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    {
        paths.push(custom);
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    for root in [
        config_home.join("opencode"),
        home.join(".opencode"),
        home.join(".config/opencode"),
    ] {
        for name in ["opencode.json", "opencode.jsonc"] {
            paths.push(root.join(name));
        }
    }

    for path in paths {
        if !path.is_file() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(config) = json5::from_str::<Value>(&contents) else {
            continue;
        };
        let Some(mcp) = config.get("mcp").and_then(Value::as_object) else {
            continue;
        };
        // Two shapes: `mcp.servers.<name>` map, or direct `mcp.<name>` entries
        // (meta keys `servers`/`timeout`/`enabled` excluded) — same rule as
        // `collect_opencode_mcp_names` in acp/mod.rs.
        if let Some(servers) = mcp.get("servers").and_then(Value::as_object) {
            push_mcp_entries(servers, HarnessId::Opencode, &path, push);
        }
        for (name, value) in mcp {
            if matches!(name.as_str(), "servers" | "timeout" | "enabled") || !value.is_object() {
                continue;
            }
            let enabled = value
                .get("enabled")
                .and_then(Value::as_bool)
                .or_else(|| value.get("disabled").and_then(Value::as_bool).map(|d| !d));
            push(
                ExtKind::McpServer,
                name.clone(),
                name.clone(),
                ExtSource {
                    agent: HarnessId::Opencode,
                    path: path.clone(),
                    enabled,
                },
            );
        }
    }
}

fn scan_plugins(home: &Path, push: &mut Push<'_>) {
    scan_claude_plugins(home, push);
    scan_copilot_plugins(home, push);
}

/// `~/.claude/plugins/installed_plugins.json` → `name@marketplace` keys.
fn scan_claude_plugins(home: &Path, push: &mut Push<'_>) {
    let path = home.join(".claude/plugins/installed_plugins.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    let Some(plugins) = config.get("plugins").and_then(Value::as_object) else {
        return;
    };
    for id in plugins.keys() {
        push(
            ExtKind::Plugin,
            id.clone(),
            id.clone(),
            ExtSource {
                agent: HarnessId::ClaudeCode,
                path: path.clone(),
                enabled: None,
            },
        );
    }
}

/// `~/.copilot/installed-plugins/<name>` directories.
fn scan_copilot_plugins(home: &Path, push: &mut Push<'_>) {
    let dir = home.join(".copilot/installed-plugins");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        push(
            ExtKind::Plugin,
            name.clone(),
            name,
            ExtSource {
                agent: HarnessId::Copilot,
                path: entry.path(),
                enabled: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn find<'a>(items: &'a [ExtItem], kind: ExtKind, id: &str) -> Option<&'a ExtItem> {
        items.iter().find(|i| i.kind == kind && i.id == id)
    }

    #[test]
    fn scans_claude_mcp_and_skills() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            &home.join(".claude.json"),
            r#"{"mcpServers":{"codegraph":{"command":"codegraph"},"gh":{"command":"gh"}}}"#,
        );
        write(
            &home.join(".claude/skills/demo/SKILL.md"),
            "---\nname: demo-skill\ndescription: Does things\n---\nbody\n",
        );
        write(&home.join(".claude/skills/not-a-skill/README.md"), "nope");

        let items = scan(home);
        let mcp = find(&items, ExtKind::McpServer, "codegraph").unwrap();
        assert!(mcp.available_to(HarnessId::ClaudeCode));
        assert_eq!(mcp.hint_for(HarnessId::ClaudeCode), None);
        let skill = find(&items, ExtKind::Skill, "demo").unwrap();
        assert_eq!(skill.name, "demo-skill");
        assert!(find(&items, ExtKind::Skill, "not-a-skill").is_none());
    }

    #[test]
    fn dedups_skill_across_agents() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        for rel in [
            ".agents/skills/shared/SKILL.md",
            ".codex/skills/own/SKILL.md",
        ] {
            write(&home.join(rel), "---\nname: x\n---\n");
        }
        let items = scan(home);
        let shared = find(&items, ExtKind::Skill, "shared").unwrap();
        // .agents/skills feeds Devin + Copilot only (evidence-based list).
        assert!(shared.available_to(HarnessId::Devin));
        assert!(shared.available_to(HarnessId::Copilot));
        assert!(!shared.available_to(HarnessId::Codex));
        assert_eq!(shared.sources.len(), 2);
        let own = find(&items, ExtKind::Skill, "own").unwrap();
        assert!(own.available_to(HarnessId::Codex));
    }

    #[test]
    fn scans_opencode_mcp_shapes_and_disabled_hint() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            &home.join(".config/opencode/opencode.json"),
            r#"{"mcp":{"servers":{"supabase":{"type":"remote","url":"https://x"}},"vercel":{"type":"remote","url":"https://y","disabled":true},"timeout":5000}}"#,
        );
        let items = scan(home);
        let supabase = find(&items, ExtKind::McpServer, "supabase").unwrap();
        assert_eq!(supabase.hint_for(HarnessId::Opencode), None);
        let vercel = find(&items, ExtKind::McpServer, "vercel").unwrap();
        assert_eq!(vercel.hint_for(HarnessId::Opencode), Some(false));
        // Meta keys must not become servers.
        assert!(find(&items, ExtKind::McpServer, "timeout").is_none());
    }

    #[test]
    fn scans_codex_mcp_tables() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            &home.join(".codex/config.toml"),
            "model = \"gpt-5\"\n\n[mcp_servers.codegraph]\ncommand = \"codegraph\"\n\n[mcp_servers.\"my server\"]\nurl = \"https://x\"\nenabled = false\n\n[other]\nx = 1\n",
        );
        let items = scan(home);
        let cg = find(&items, ExtKind::McpServer, "codegraph").unwrap();
        assert!(cg.available_to(HarnessId::Codex));
        let srv = find(&items, ExtKind::McpServer, "my server").unwrap();
        assert_eq!(srv.hint_for(HarnessId::Codex), Some(false));
        assert!(find(&items, ExtKind::McpServer, "other").is_none());
    }

    #[test]
    fn scans_claude_plugins() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            r#"{"version":2,"plugins":{"ecc@ecc":[{"version":"2.2.3"}]}}"#,
        );
        let items = scan(home);
        let plugin = find(&items, ExtKind::Plugin, "ecc@ecc").unwrap();
        assert!(plugin.available_to(HarnessId::ClaudeCode));
    }
}
