use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
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
