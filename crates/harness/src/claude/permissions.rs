//! Claude Code permissions and sandbox translation.
//!
//! Maps [`komet_proto::ClaudeSandbox`] and permission rules into Claude's
//! `--settings` format (`permissions`, `sandbox`, `credentials`, `network`),
//! and extracts [`komet_proto::PermissionKind`] from incoming `can_use_tool` requests.

use serde_json::Value;
use komet_proto::{ClaudeSandbox, PermissionChoice, PermissionKind, Scope};

/// Build the `permissions` and `sandbox` maps for Claude CLI's `--settings` JSON.
pub fn build_claude_settings_maps(
    c: &ClaudeSandbox,
) -> (serde_json::Map<String, Value>, serde_json::Map<String, Value>) {
    let mut perms = serde_json::Map::new();
    perms.insert("defaultMode".into(), Value::String("default".into()));

    let mut deny: Vec<Value> = c
        .excluded_commands
        .iter()
        .map(|cmd| Value::String(format!("Bash({cmd}:*)")))
        .collect();

    for path in c.filesystem.deny.iter().chain(&c.filesystem.deny_write) {
        deny.push(Value::String(format!("Edit({path})")));
    }

    // Paseo `denyRead` — refuse reads (e.g. ~/.ssh, ~/.aws). Maps to
    // Claude's `Read(path)` permission rule, distinct from Edit.
    for path in &c.filesystem.deny_read {
        deny.push(Value::String(format!("Read({path})")));
    }

    if !deny.is_empty() {
        perms.insert("deny".into(), Value::Array(deny));
    }

    let extra_dirs: Vec<_> = c
        .filesystem
        .allow
        .iter()
        .chain(&c.filesystem.allow_read)
        .chain(&c.filesystem.allow_write)
        .cloned()
        .collect();
    let mut combined_dirs = extra_dirs;
    combined_dirs.extend(c.additional_directories.clone());
    combined_dirs.sort();
    combined_dirs.dedup();
    if !combined_dirs.is_empty() {
        perms.insert(
            "additionalDirectories".into(),
            Value::Array(combined_dirs.into_iter().map(Value::String).collect()),
        );
    }

    if !c.allowed_tools.is_empty() {
        perms.insert(
            "allow".into(),
            Value::Array(
                c.allowed_tools
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect(),
            ),
        );
    }

    if !c.disallowed_tools.is_empty() {
        let mut deny_arr = perms
            .get("deny")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for t in &c.disallowed_tools {
            deny_arr.push(Value::String(t.clone()));
        }
        perms.insert("deny".into(), Value::Array(deny_arr));
    }

    // ── settings.sandbox (unified) ────────────────────────────────
    let mut sandbox = serde_json::Map::new();
    if let Some(fs_sandbox) = c.settings.sandbox.clone() {
        if let Some(obj) = fs_sandbox.as_object() {
            for (k, v) in obj {
                // Skip network keys from settings.sandbox — overwritten
                // by the generated restrictions below.
                if k != "network" {
                    sandbox.insert(k.clone(), v.clone());
                }
            }
        }
    }

    let has_network =
        !c.network.allowed_hosts.is_empty() || !c.network.denied_hosts.is_empty();
    if has_network || c.network.strict_allowlist.is_some() {
        let mut network = serde_json::Map::new();
        if !c.network.allowed_hosts.is_empty() {
            network.insert(
                "allowedDomains".into(),
                Value::Array(
                    c.network
                        .allowed_hosts
                        .iter()
                        .map(|h| Value::String(h.clone()))
                        .collect(),
                ),
            );
        }
        if !c.network.denied_hosts.is_empty() {
            network.insert(
                "deniedDomains".into(),
                Value::Array(
                    c.network
                        .denied_hosts
                        .iter()
                        .map(|h| Value::String(h.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(strict) = c.network.strict_allowlist {
            network.insert("strictAllowlist".into(), Value::Bool(strict));
        }
        sandbox.insert("network".into(), Value::Object(network));
    }

    // Claude credentials — maps to sandbox.credentials.files.
    if !c.credentials.is_empty() {
        let cred_arr: Vec<Value> = c
            .credentials
            .iter()
            .map(|p| Value::String(p.clone()))
            .collect();
        let mut cred_obj = serde_json::Map::new();
        cred_obj.insert("files".into(), Value::Array(cred_arr));
        sandbox.insert("credentials".into(), Value::Object(cred_obj));
    }

    // A2 — fail closed: while the sandbox is active, commands that can't
    // be sandboxed must FAIL rather than fall back to the regular
    // permission flow. Allow the escape hatch only when the caller asks.
    sandbox.insert(
        "allowUnsandboxedCommands".into(),
        Value::Bool(c.allow_unsandboxed_commands),
    );
    sandbox.insert(
        "failIfUnavailable".into(),
        Value::Bool(c.fail_if_unavailable),
    );

    if let Some(extra) = c.settings_permissions.as_object() {
        for (k, v) in extra {
            perms.insert(k.clone(), v.clone());
        }
    }

    // Claude native permission rules → settings.permissions {allow,ask,deny}:[].
    for rule in &c.permissions {
        let entry = format!("{}({})", rule.action, rule.resource);
        let list_key = match rule.effect {
            komet_proto::Perm::Allow => "allow",
            komet_proto::Perm::Ask => "ask",
            komet_proto::Perm::Deny => "deny",
        };
        perms
            .entry(list_key.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(Value::String(entry));
    }

    (perms, sandbox)
}

/// Extract [`PermissionKind`], summary, and default choices from a tool call payload.
pub fn parse_tool_permission(
    tool_name: &str,
    input: &Value,
) -> (PermissionKind, String, Vec<PermissionChoice>) {
    let choices = vec![
        PermissionChoice::Allow,
        PermissionChoice::AllowAlways {
            scope: Scope::Chat,
        },
        PermissionChoice::Deny,
    ];

    match tool_name {
        "Bash" | "bash" | "execute_command" => {
            let cmd = input
                .get("command")
                .or_else(|| input.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let summary = if cmd.is_empty() {
                "Run command".into()
            } else {
                format!("Run `{cmd}`")
            };
            (
                PermissionKind::Command {
                    cmdline: cmd.to_owned(),
                },
                summary,
                choices,
            )
        }
        "Edit" | "write_file" | "create_file" | "str_replace_editor" => {
            let path = input
                .get("path")
                .or_else(|| input.get("file_path"))
                .or_else(|| input.get("target_file"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let summary = if path.is_empty() {
                "Write file".into()
            } else {
                format!("Write `{path}`")
            };
            (
                PermissionKind::FileWrite {
                    path: path.to_owned(),
                },
                summary,
                choices,
            )
        }
        other => {
            let summary = format!("Execute tool `{other}`");
            (
                PermissionKind::Tool {
                    name: other.to_owned(),
                },
                summary,
                choices,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bash_command_permission() {
        let input = json!({ "command": "git push origin main" });
        let (kind, summary, choices) = parse_tool_permission("Bash", &input);
        assert_eq!(
            kind,
            PermissionKind::Command {
                cmdline: "git push origin main".into()
            }
        );
        assert_eq!(summary, "Run `git push origin main`");
        assert_eq!(choices.len(), 3);
    }

    #[test]
    fn parses_edit_file_permission() {
        let input = json!({ "path": "src/main.rs", "content": "fn main() {}" });
        let (kind, summary, _) = parse_tool_permission("Edit", &input);
        assert_eq!(
            kind,
            PermissionKind::FileWrite {
                path: "src/main.rs".into()
            }
        );
        assert_eq!(summary, "Write `src/main.rs`");
    }

    #[test]
    fn parses_generic_tool_permission() {
        let input = json!({ "key": "value" });
        let (kind, summary, _) = parse_tool_permission("custom_mcp_tool", &input);
        assert_eq!(
            kind,
            PermissionKind::Tool {
                name: "custom_mcp_tool".into()
            }
        );
        assert_eq!(summary, "Execute tool `custom_mcp_tool`");
    }
}
