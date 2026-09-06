use super::policy::{Decision, McpPolicy};

#[test]
fn policy_unknown_tool_asks_and_deny_wins() {
    let mut pol = McpPolicy::default(); // inconnue → ask
    assert_eq!(pol.check("github", "unknown_tool"), Decision::Ask);
    pol.set("github", "create_issue", Decision::Allow);
    pol.set("github", "create_issue", Decision::Deny);
    assert_eq!(pol.check("github", "create_issue"), Decision::Deny); // deny prioritaire
}

#[test]
fn policy_write_asks_by_default() {
    let pol = McpPolicy::default();
    assert_eq!(pol.check("github", "create_issue"), Decision::Ask); // écriture
    assert_eq!(pol.check("github", "list_issues"), Decision::Allow); // lecture approuvée → allow
}
