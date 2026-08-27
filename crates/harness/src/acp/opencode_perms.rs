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
//! STATUS: this module is the pure translation seam only. ACP exposes no
//! permission config surface today (nothing in `session/new`'s
//! `configOptions`, no dedicated method), and pointing `OPENCODE_CONFIG` at
//! a generated file is unverified against merge semantics — wiring that up
//! without live validation risks clobbering a user's own config. Callers
//! (engine/UI) should write this section into an `opencode.json` overlay
//! once that path is validated; the generator here is stable and tested.

use komet_proto::{BashPerms, OpenCodePerms, Perm};

/// The full `"permission"` value for an `opencode.json` overlay, key order
/// preserved exactly as declared.
pub fn permission_config(perms: &OpenCodePerms) -> String {
    let mut out = String::from("{");
    out.push_str("\"bash\":{");
    out.push_str(&bash_patterns_json(&perms.bash));
    out.push('}');
    for (tool, perm) in &perms.unscoped_actions {
        out.push(',');
        push_key(&mut out, tool);
        out.push(':');
        out.push_str(perm_str(*perm));
    }
    out.push('}');
    out
}

/// The `"permission"` section wrapped as a complete `opencode.json` document.
pub fn opencode_config_document(perms: &OpenCodePerms) -> String {
    format!("{{\"permission\":{}}}", permission_config(perms))
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
}
