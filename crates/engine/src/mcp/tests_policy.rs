use super::policy::{Decision, McpPolicy, secure_summary};

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

#[test]
fn memory_writes_always_ask_even_after_allow_session() {
    let mut pol = McpPolicy::default();
    // Simulate the user answering "Allow for session" on memory.note.
    pol.memoize("komet", "memory.note", Decision::Allow);
    pol.memoize("komet", "memory.claim", Decision::Allow);
    assert_eq!(pol.decide("komet", "memory.note", false), Decision::Ask);
    assert_eq!(pol.decide("komet", "memory.claim", false), Decision::Ask);
    assert_eq!(pol.check("komet", "memory.note"), Decision::Ask);
    // Other tools keep the session memo.
    pol.memoize("komet", "list_sessions", Decision::Allow);
    assert_eq!(pol.decide("komet", "list_sessions", true), Decision::Allow);
    // An explicit Deny rule still wins.
    pol.set_rule("komet", "memory.note", Decision::Deny);
    assert_eq!(pol.decide("komet", "memory.note", false), Decision::Deny);
}

#[test]
fn memory_note_summary_shows_the_fact_text() {
    // The approval prompt must show WHAT is being saved, not just the tool
    // name — the note is injected into other agents' prompts later.
    let args = serde_json::json!({
        "kind": "decision",
        "content": "Use a bounded writer for the store",
        "scope": "project",
    });
    let summary = secure_summary("komet", "memory.note", &args);
    assert!(summary.contains("memory.note"));
    assert!(summary.contains("Use a bounded writer for the store"));
}
