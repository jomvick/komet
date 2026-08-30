//! Codex permissions and sandbox policy translation.
//!
//! Maps [`komet_proto::CodexSandbox`] and approval policies into Codex app-server
//! wire payloads (`sandboxPolicy`, `approvalPolicy`), and extracts
//! [`komet_proto::PermissionKind`] from incoming approval requests.

use serde_json::{Value, json};
use komet_proto::{
    ApprovalPolicy, CodexSandbox, PermissionChoice, PermissionKind, SandboxMode, Scope,
};

/// Build the `(approval_policy, sandbox_mode_str, sandbox_policy)` tuple for Codex `turn/start`.
pub fn build_codex_policies(
    cx: &CodexSandbox,
    cwd: &str,
) -> (Value, &'static str, Value) {
    let mode = cx.sandbox_mode.unwrap_or(SandboxMode::WorkspaceWrite);
    let mut policy = serde_json::Map::new();
    policy.insert(
        "type".into(),
        match mode {
            SandboxMode::ReadOnly => "readOnly",
            SandboxMode::WorkspaceWrite => "workspaceWrite",
            SandboxMode::DangerFullAccess => "dangerFullAccess",
        }
        .into(),
    );
    if cx.network_access {
        policy.insert("networkAccess".into(), true.into());
    }
    if !cx.writable_roots.is_empty() {
        policy.insert(
            "writableRoots".into(),
            json!(
                cx.writable_roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
            ),
        );
    }
    if let Some(ws) = &cx.sandbox_workspace_write {
        if ws.exclude_slash_tmp {
            policy.insert("excludeSlashTmp".into(), true.into());
        }
        if ws.exclude_tmpdir_env_var {
            policy.insert("excludeTmpdirEnvVar".into(), true.into());
        }
    }
    // Codex filesystem policy map (B3 protected subpaths default + explicit entries).
    {
        let mut fs_map = serde_json::Map::new();
        if !cx.filesystem.is_empty() {
            for entry in &cx.filesystem {
                let access_str = match entry.access {
                    komet_proto::FSAccess::Read => "read",
                    komet_proto::FSAccess::Write => "write",
                    komet_proto::FSAccess::Deny => "deny",
                };
                fs_map.insert(entry.path.clone(), Value::String(access_str.into()));
            }
        } else if mode == SandboxMode::WorkspaceWrite && !cwd.is_empty() {
            let defaults = komet_proto::default_read_only_subpaths_for_root(cwd);
            for entry in defaults {
                fs_map.insert(entry.path, Value::String("read".into()));
            }
        }
        if !fs_map.is_empty() {
            policy.insert("filesystem".into(), Value::Object(fs_map));
        }
    }
    // Codex shell environment policy — excludes env vars from sandbox (B2 defaults).
    {
        let effective_exclude: Vec<String> = match &cx.shell_env_policy {
            Some(pol) if !pol.exclude.is_empty() => pol.exclude.clone(),
            _ if mode != SandboxMode::DangerFullAccess => {
                komet_proto::DEFAULT_CODEX_SHELL_EXCLUDE
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            }
            _ => vec![],
        };
        if !effective_exclude.is_empty() {
            let exclude_arr: Vec<Value> =
                effective_exclude.into_iter().map(Value::String).collect();
            let mut sep_obj = serde_json::Map::new();
            sep_obj.insert("exclude".into(), Value::Array(exclude_arr));
            policy.insert("shellEnvironmentPolicy".into(), Value::Object(sep_obj));
        }
    }

    let mode_str = match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    };

    let approval = match cx
        .approval_policy
        .as_ref()
        .unwrap_or(&ApprovalPolicy::Never)
    {
        ApprovalPolicy::Never => json!("never"),
        ApprovalPolicy::Untrusted => json!("untrusted"),
        ApprovalPolicy::OnRequest => json!("on-request"),
        ApprovalPolicy::Granular { ask, auto_approve } => json!({
            "kind": "granular",
            "ask": ask,
            "autoApprove": auto_approve,
        }),
    };

    (approval, mode_str, Value::Object(policy))
}

/// Extract [`PermissionKind`], summary, and default choices from a Codex approval request.
pub fn parse_approval_request(
    method: &str,
    params: &Value,
) -> (PermissionKind, String, Vec<PermissionChoice>) {
    let choices = vec![
        PermissionChoice::Allow,
        PermissionChoice::AllowAlways {
            scope: Scope::Chat,
        },
        PermissionChoice::Deny,
    ];

    if method.contains("commandExecution") {
        let command = match params.get("command") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => params
                .get("item")
                .and_then(|it| it.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let summary = if command.is_empty() {
            "Run command".into()
        } else {
            format!("Run `{command}`")
        };
        (
            PermissionKind::Command { cmdline: command },
            summary,
            choices,
        )
    } else {
        let paths: Vec<String> = params
            .get("changes")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.get("path").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let path = paths
            .first()
            .cloned()
            .or_else(|| {
                params
                    .get("path")
                    .or_else(|| params.get("filePath"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let summary = if !paths.is_empty() {
            format!("Write `{}`", paths.join(", "))
        } else if !path.is_empty() {
            format!("Write `{path}`")
        } else {
            "Write file changes".into()
        };
        (PermissionKind::FileWrite { path }, summary, choices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_execution_approval() {
        let params = json!({ "command": ["git", "push", "origin", "main"] });
        let (kind, summary, choices) =
            parse_approval_request("item/commandExecution/requestApproval", &params);
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
    fn parses_file_change_approval() {
        let params = json!({ "changes": [{ "path": "src/lib.rs" }] });
        let (kind, summary, choices) =
            parse_approval_request("item/fileChange/requestApproval", &params);
        assert_eq!(
            kind,
            PermissionKind::FileWrite {
                path: "src/lib.rs".into()
            }
        );
        assert_eq!(summary, "Write `src/lib.rs`");
        assert_eq!(choices.len(), 3);
    }
}
