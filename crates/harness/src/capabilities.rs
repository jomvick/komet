use komet_proto::HarnessId;

/// Returns `(supports, reason)`. `reason` is empty when supported, otherwise a
/// human-readable explanation for UI (French, per spec). True for `claude`
/// and `opencode` (dynamic `mcpServers` injection), false for all others.
pub fn supports_dynamic_mcp(id: HarnessId) -> (bool, &'static str) {
    match id {
        HarnessId::ClaudeCode | HarnessId::Opencode => (true, ""),
        _ => (
            false,
            "Ce provider ne supporte pas l'injection MCP dynamique",
        ),
    }
}

/// Convenience bool-only view for call sites that only need the flag.
pub fn supports_dynamic_mcp_bool(id: HarnessId) -> bool {
    supports_dynamic_mcp(id).0
}
