use komet_proto::AgentEvent;
pub fn normalize_line(line: &str) -> Option<AgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let t = v
        .get("text")
        .or_else(|| v.get("content"))
        .or_else(|| v.get("message"))?
        .as_str()?;
    Some(AgentEvent::TextDelta {
        text: t.to_string(),
    })
}
