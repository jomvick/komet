use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Secure summary for permission prompts: `Serveur: {s} Tool: {t} Arguments: {…}`
/// Masks sensitive keys (token/password/secret/authorization/bearer, case-insensitive),
/// truncates per-value to 200 chars, whole args to 500, final string to 800,
/// and redacts any raw `Bearer` (case-insensitive) to `***`.
/// Public so `doc_host` and `server` share the same implementation.
pub fn secure_summary(server: &str, tool: &str, args: &serde_json::Value) -> String {
    let mut s = format!("Serveur: {server} Tool: {tool} Arguments: ");
    let args_str = if args.is_null() || args.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        "{}".to_string()
    } else {
        let mut masked = serde_json::Map::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let lower = k.to_lowercase();
                if lower.contains("token")
                    || lower.contains("password")
                    || lower.contains("secret")
                    || lower.contains("authorization")
                    || lower.contains("bearer")
                {
                    masked.insert(k.clone(), serde_json::Value::String("***".into()));
                } else if let Some(s) = v.as_str() {
                    // String value – truncate inner string to 200 chars
                    let mut val_str = s.to_string();
                    if val_str.len() > 200 {
                        val_str.truncate(200);
                        val_str.push('…');
                    }
                    masked.insert(k.clone(), serde_json::Value::String(val_str));
                } else {
                    let mut val_str = v.to_string();
                    if val_str.len() > 200 {
                        val_str.truncate(200);
                        val_str.push('…');
                        masked.insert(k.clone(), serde_json::Value::String(val_str));
                    } else {
                        masked.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        let mut out = serde_json::to_string(&serde_json::Value::Object(masked))
            .unwrap_or_else(|_| "{}".into());
        if out.len() > 500 {
            out.truncate(500);
            out.push('…');
        }
        out
    };
    s.push_str(&args_str);
    if s.len() > 800 {
        s.truncate(800);
        s.push('…');
    }
    // Case-insensitive Bearer redaction (e.g. "Bearer", "bearer", "BEARER")
    s = replace_case_insensitive(&s, "bearer", "***");
    s
}

fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let lower_hay = haystack.to_lowercase();
    let lower_needle = needle.to_lowercase();
    let mut result = String::with_capacity(haystack.len());
    let mut last = 0usize;
    let mut search_start = 0usize;
    while let Some(pos) = lower_hay[search_start..].find(&lower_needle) {
        let abs_pos = search_start + pos;
        result.push_str(&haystack[last..abs_pos]);
        result.push_str(replacement);
        last = abs_pos + needle.len();
        search_start = abs_pos + needle.len();
    }
    result.push_str(&haystack[last..]);
    result
}

pub struct McpPolicy {
    rules: HashMap<(String, String), Decision>,
    session_memo: HashMap<(String, String), Decision>,
}

fn is_read_tool(tool: &str) -> bool {
    tool.starts_with("list_")
        || tool.starts_with("get_")
        || tool.starts_with("search_")
        || tool.starts_with("read_")
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self::komet_default()
    }
}

impl McpPolicy {
    pub fn komet_default() -> Self {
        Self {
            rules: HashMap::new(),
            session_memo: HashMap::new(),
        }
    }

    pub fn set_rule(&mut self, server: &str, tool: &str, d: Decision) {
        let key = (server.to_string(), tool.to_string());
        // Deny is sticky: once denied, Allow cannot overwrite it (deny priority)
        if d == Decision::Deny {
            self.rules.insert(key, d);
        } else if self.rules.get(&key) == Some(&Decision::Deny) {
            // keep Deny, do not overwrite with Allow/Ask
        } else {
            self.rules.insert(key, d);
        }
    }

    /// Alias for `set_rule` – used by external policy tests (`pol.set(...)`)
    pub fn set(&mut self, server: &str, tool: &str, d: Decision) {
        self.set_rule(server, tool, d);
    }

    pub fn memoize(&mut self, server: &str, tool: &str, d: Decision) {
        self.session_memo.insert((server.into(), tool.into()), d);
    }

    pub fn decide(&self, server: &str, tool: &str, readonly: bool) -> Decision {
        let key = (server.to_string(), tool.to_string());
        if let Some(d) = self.rules.get(&key) {
            return *d;
        }
        if let Some(d) = self.session_memo.get(&key) {
            return *d;
        }
        if readonly {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }

    /// Server+tool check without explicit readonly flag – infers readonly from
    /// tool naming convention (list_*, get_* → read → Allow, others → Ask).
    /// Unknown tools default to Ask, Deny always wins.
    pub fn check(&self, server: &str, tool: &str) -> Decision {
        let key = (server.to_string(), tool.to_string());
        if let Some(d) = self.rules.get(&key) {
            return *d;
        }
        if let Some(d) = self.session_memo.get(&key) {
            return *d;
        }
        if is_read_tool(tool) {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, McpPolicy};

    #[test]
    fn default_policy_allows_reads_asks_mutations_memoizes_session() {
        let mut p = McpPolicy::komet_default();
        assert_eq!(p.decide("komet", "list_spaces", true), Decision::Allow);
        assert_eq!(p.decide("komet", "write_file", false), Decision::Ask);
        p.memoize("komet", "write_file", Decision::Allow);
        assert_eq!(p.decide("komet", "write_file", false), Decision::Allow);
        p.set_rule("komet", "write_file", Decision::Deny);
        p.memoize("komet", "write_file", Decision::Allow);
        assert_eq!(p.decide("komet", "write_file", false), Decision::Deny);
    }
}
