use komet_proto::HarnessId;

/// Returns `(supports, reason)`. `reason` is empty when supported, otherwise a
/// human-readable explanation for UI (French, per spec).
///
/// True where Komet actually injects `mcpServers`: Claude (`--mcp-config`)
/// and ACP agents (`session/new`). `Mock` is the test double. Codex / Cursor
/// / Antigravity stay false — no injection path.
pub fn supports_dynamic_mcp(id: HarnessId) -> (bool, &'static str) {
    match id {
        HarnessId::ClaudeCode
        | HarnessId::Opencode
        | HarnessId::Grok
        | HarnessId::Hermes
        | HarnessId::Pi
        | HarnessId::Mock => (true, ""),
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
