//! Translation of [`OpenCodePerms`] into opencode's native `permission`
//! config section (`opencode.json` shape):
//!
//! ```json
//! { "permission": { "bash": { "*": "ask", "git *": "allow" },
//!                   "webfetch": "ask", ... } }
//! ```
//!
//! WHY a hand-built string: serde_json's object map sorts keys (no
//! `preserve_order` feature), which would silently reorder the bash pattern
//! table — and for opencode, order IS semantics (the LAST matching pattern
//! wins, see [`komet_proto::BashPerms::resolve`]). Building the JSON text in
//! document order keeps the round-trip faithful.
//!
//! STATUS: wired. `AcpHarness::run` writes this section as a temp
//! `opencode.json` overlay and passes its content via `OPENCODE_CONFIG_CONTENT`
//! at spawn time (`crates/harness/src/acp/mod.rs`) — final-precedence env var,
//! so a project-level `opencode.json` cannot silently weaken the generated
//! permissions (falls back to the `OPENCODE_CONFIG` file-path env var only if
//! reading the overlay back fails). The user's own config file is never
//! rewritten.

use komet_proto::{BashPerms, OpenCodePerms, Perm};

/// The full `"permission"` value for an `opencode.json` overlay, key order
/// preserved exactly as declared. `bash` renders as its ordered pattern map
/// and always appears; the bare per-tool fields (`read`/`edit`/
/// `external_directory`/`webfetch`/`websearch`) render as their bare perm
/// when set. A bare field that also appears in `unscoped_actions` wins — the
/// map is order-less, so emitting a duplicate key would make the document
/// ambiguous.
pub fn permission_config(perms: &OpenCodePerms) -> String {
    let mut out = String::from("{\"bash\":{");
    out.push_str(&bash_patterns_json(&perms.bash));
    out.push('}');
    let dedicated = [
        ("read", perms.read),
        ("edit", perms.edit),
        ("external_directory", perms.external_directory),
        ("webfetch", perms.webfetch),
        ("websearch", perms.websearch),
        ("glob", perms.glob),
        ("grep", perms.grep),
        ("skill", perms.skill),
        ("lsp", perms.lsp),
        ("question", perms.question),
        ("execute", perms.execute),
        ("task", perms.task),
        ("doom_loop", perms.doom_loop),
    ];
    // C3 — sensitive read deny: when no explicit `read` perm was set, emit a
    // granular `read` pattern map that denies the sensitive paths while
    // keeping ambient reads allowed. An explicit user `read` wins (their
    // choice, no silent override in either direction).
    if perms.read.is_none() && !perms.sensitive_read_deny.is_empty() {
        out.push_str(",\"read\":{");
        let mut first = true;
        for pattern in &perms.sensitive_read_deny {
            if !first {
                out.push(',');
            }
            first = false;
            push_key(&mut out, pattern);
            out.push_str(":\"deny\"");
        }
        out.push_str(",\"*\":\"allow\"}");
    }
    for (tool, perm) in &perms.unscoped_actions {
        if dedicated.iter().any(|(n, p)| *n == tool && p.is_some()) {
            // The dedicated field renders this key below; skip the duplicate.
            continue;
        }
        out.push(',');
        push_key(&mut out, tool);
        out.push(':');
        out.push_str(perm_str(*perm));
    }
    for (tool, perm) in dedicated {
        if let Some(p) = perm {
            out.push(',');
            push_key(&mut out, tool);
            out.push(':');
            out.push_str(perm_str(p));
        }
    }
    out.push('}');
    out
}

/// The `"permission"` section wrapped as a complete `opencode.json` document.
/// When `opencode_sandbox_runtime` is enabled the overlay also declares the
/// `opencode-sandbox` plugin so the runtime is active; the actual
/// `sandbox.json` is written to the workspace by the harness (see
/// `sandbox_runtime_config`).
pub fn opencode_config_document(perms: &OpenCodePerms) -> String {
    let perm = permission_config(perms);
    if perms.opencode_sandbox_runtime == Some(true) {
        format!("{{\"permission\":{perm},\"plugin\":[\"opencode-sandbox\"]}}")
    } else {
        format!("{{\"permission\":{perm}}}")
    }
}

/// Content for `.opencode/sandbox.json` consumed by
/// `kszarek/opencode-sandbox-plugin`. `None` when the opt-in flag is off.
pub fn sandbox_runtime_config(perms: &OpenCodePerms, cwd: &str) -> Option<String> {
    if perms.opencode_sandbox_runtime != Some(true) {
        return None;
    }
    let mut deny_read: Vec<String> = perms.sensitive_read_deny.clone();
    deny_read.extend(perms.read_only_paths.clone());
    if deny_read.is_empty() {
        deny_read = komet_proto::OPCODE_SENSITIVE_READ_DENY.iter().map(|s| s.to_string()).collect();
    }
    let allow_write = vec![cwd.to_string(), "/tmp".to_string()];
    let cfg = serde_json::json!({
        "filesystem": {
            "denyRead": deny_read,
            "allowWrite": allow_write
        }
    });
    Some(serde_json::to_string_pretty(&cfg).unwrap())
}

fn bash_patterns_json(bash: &BashPerms) -> String {
    let mut out = String::new();
    let mut first = true;
    for (pattern, perm) in &bash.patterns {
        if !first {
            out.push(',');
        }
        first = false;
        push_key(&mut out, pattern);
        out.push(':');
        out.push_str(perm_str(*perm));
    }
    out
}

fn perm_str(p: Perm) -> &'static str {
    match p {
        Perm::Allow => "\"allow\"",
        Perm::Ask => "\"ask\"",
        Perm::Deny => "\"deny\"",
    }
}

/// JSON string literal with proper escaping (serde_json does the work; we
/// only need the quoted text, not a `Value`).
fn push_key(out: &mut String, key: &str) {
    let quoted = serde_json::to_string(key).expect("string serialization cannot fail");
    out.push_str(&quoted);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(patterns: Vec<(&str, Perm)>, unscoped: Vec<(&str, Perm)>) -> OpenCodePerms {
        OpenCodePerms {
            bash: BashPerms {
                patterns: patterns
                    .into_iter()
                    .map(|(k, p)| (k.to_owned(), p))
                    .collect(),
            },
            unscoped_actions: unscoped
                .into_iter()
                .map(|(k, p)| (k.to_owned(), p))
                .collect(),
            read: None,
            edit: None,
            external_directory: None,
            webfetch: None,
            websearch: None,
            glob: None,
            grep: None,
            skill: None,
            lsp: None,
            question: None,
            execute: None,
            task: None,
            doom_loop: None,
            sensitive_read_deny: vec![],
            opencode_sandbox_runtime: None,
            read_only_paths: vec![],
        }
    }

    #[test]
    fn preserves_pattern_order_and_last_match_semantics() {
        // Paseo canonical default: "*" FIRST as fallback, more specific
        // patterns after — the LAST match wins.
        let p = perms(
            vec![
                ("*", Perm::Ask),
                ("git *", Perm::Allow),
                ("rm -rf *", Perm::Deny),
            ],
            vec![],
        );
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{"*":"ask","git *":"allow","rm -rf *":"deny"}}"#
        );
        // The resolver agrees: last match wins over the "*" fallback.
        assert_eq!(p.bash.resolve("git push"), Some(Perm::Allow));
        assert_eq!(p.bash.resolve("rm -rf /"), Some(Perm::Deny));
        assert_eq!(p.bash.resolve("ls -la"), Some(Perm::Ask));
    }

    #[test]
    fn unscoped_actions_render_as_tool_keys() {
        let p = perms(
            vec![],
            vec![("webfetch", Perm::Ask), ("todowrite", Perm::Allow)],
        );
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{},"todowrite":"allow","webfetch":"ask"}"#
        );
    }

    #[test]
    fn keys_are_escaped() {
        let p = perms(vec![("say \"hi\"*", Perm::Deny)], vec![]);
        assert_eq!(permission_config(&p), r#"{"bash":{"say \"hi\"*":"deny"}}"#);
    }

    #[test]
    fn document_wraps_permission_section() {
        let p = perms(vec![("*", Perm::Ask)], vec![]);
        assert_eq!(
            opencode_config_document(&p),
            r#"{"permission":{"bash":{"*":"ask"}}}"#
        );
    }
#[test]
    fn bare_tool_fields_render_at_their_top_level_keys() {
        let p = OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".to_owned(), Perm::Deny)],
            },
            unscoped_actions: Default::default(),
            read: Some(Perm::Allow),
            edit: Some(Perm::Allow),
            external_directory: Some(Perm::Deny),
            webfetch: Some(Perm::Deny),
            websearch: Some(Perm::Ask),
            ..Default::default()
        };
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{"*":"deny"},"read":"allow","edit":"allow","external_directory":"deny","webfetch":"deny","websearch":"ask"}"#
        );
    }

    #[test]
    fn dedicated_bare_field_beats_unscoped_action_on_same_key() {
        // webfetch with a dedicated value AND an unscoped entry: the dedicated
        // field wins; the unscoped duplicate is not emitted.
        let p = OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".to_owned(), Perm::Ask)],
            },
            unscoped_actions: [("webfetch".to_owned(), Perm::Allow)].into(),
            read: None,
            edit: None,
            external_directory: None,
            webfetch: Some(Perm::Deny),
            websearch: None,
            ..Default::default()
        };
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{"*":"ask"},"webfetch":"deny"}"#
        );
    }

    #[test]
    fn sensitive_read_deny_emits_granular_map_when_read_unset() {
        // C3: restricted levels populate sensitive_read_deny; with no explicit
        // `read`, the overlay denies the sensitive paths and keeps ambient
        // reads allowed via the trailing "*":"allow".
        let p = OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".to_owned(), Perm::Ask)],
            },
            unscoped_actions: Default::default(),
            sensitive_read_deny: vec![".env".to_owned(), "~/.ssh/**".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{"*":"ask"},"read":{".env":"deny","~/.ssh/**":"deny","*":"allow"}}"#
        );
    }

    #[test]
    fn explicit_read_beats_sensitive_deny_defaults() {
        // An explicit user `read` wins — no silent override in either direction.
        let p = OpenCodePerms {
            bash: BashPerms {
                patterns: vec![("*".to_owned(), Perm::Ask)],
            },
            unscoped_actions: Default::default(),
            read: Some(Perm::Allow),
            sensitive_read_deny: vec![".env".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            permission_config(&p),
            r#"{"bash":{"*":"ask"},"read":"allow"}"#
        );
    }
}
