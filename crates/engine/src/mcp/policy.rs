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

impl McpPolicy {
    pub fn komet_default() -> Self {
        Self {
            rules: HashMap::new(),
            session_memo: HashMap::new(),
        }
    }

    pub fn set_rule(&mut self, server: &str, tool: &str, d: Decision) {
        self.rules.insert((server.into(), tool.into()), d);
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
