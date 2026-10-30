use komet_proto::{
    CompactionSurface, HarnessId, HarnessInteractionSurface, PlanSurface, ProgressSurface,
    QuestionSurface, SubagentSurface,
};

/// The one ACP-shaped declaration, parameterised by the subagent case (the only
/// column that differs across the ACP family today).
///
/// Questions are `PermissionShaped` and compaction `None` for every ACP agent:
/// `elicitation/create` and the compaction signals are draft RFDs, and no agent
/// has been verified emitting them (Gate B), so Komet advertises neither the
/// capability nor the affordance.
const fn acp_surface(subagents: SubagentSurface) -> HarnessInteractionSurface {
    HarnessInteractionSurface {
        questions: QuestionSurface::PermissionShaped,
        compaction: CompactionSurface::None,
        subagents,
        plans: PlanSurface::ToolChip,
        tool_progress: ProgressSurface::Streaming,
    }
}

/// What one harness's interaction surface is, as the adapters actually stand
/// today. This is the single source of truth: `HarnessDescriptor.surface` and
/// Settings project this table rather than declaring a second copy, and a new
/// `HarnessId` cannot be added without a row (the match is exhaustive on
/// purpose — an absent declaration must not degrade into "nothing to show").
///
/// Each value says what the **adapter maps**, per the matrix in
/// `docs/superpowers/plans/2026-10-27-agent-transparency.md` §2; a `Streaming`
/// progress row for an ACP agent means the intermediate content is carried, not
/// that a given CLI was observed sending it.
pub fn interaction_surface(id: HarnessId) -> HarnessInteractionSurface {
    match id {
        HarnessId::ClaudeCode => HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::LifecycleWithSummary,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::ToolChip,
            // The CLI gives no partial tool stdout: declared, never simulated.
            tool_progress: ProgressSurface::None,
        },
        HarnessId::Codex => HarnessInteractionSurface {
            // `item/tool/requestUserInput` round-trips through `request_input`
            // as a native question; approvals stay on the permission bridge.
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::BoundaryOnly,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
        HarnessId::Cursor => HarnessInteractionSurface {
            questions: QuestionSurface::None,
            compaction: CompactionSurface::BoundaryOnly,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::ToolChip,
            // The SDK shim carries no partial tool content.
            tool_progress: ProgressSurface::Final,
        },
        HarnessId::Grok | HarnessId::Opencode => acp_surface(SubagentSurface::NestedSession),
        HarnessId::Devin
        | HarnessId::Copilot
        | HarnessId::Hermes
        | HarnessId::Pi
        | HarnessId::Cline
        | HarnessId::Qoder => acp_surface(SubagentSurface::TaggedIfEmitted),
        HarnessId::Antigravity => HarnessInteractionSurface {
            // `ask_question` reaches the user as a visible notice only: `agy -p`
            // is a one-shot stream with no channel to answer an in-run question
            // (transparency plan Task 6, Step 5) — declared `None`, not papered
            // over with an unanswerable panel.
            questions: QuestionSurface::None,
            compaction: CompactionSurface::None,
            subagents: SubagentSurface::None,
            plans: PlanSurface::ToolChip,
            tool_progress: ProgressSurface::Final,
        },
        HarnessId::Muse => HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::Lifecycle,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
        HarnessId::Mock => HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::LifecycleWithSummary,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
    }
}

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
        // Cline accepts `mcpServers` at ACP `session/new` like the other
        // native ACP agents (verified live against `cline --acp` 3.0.62).
        | HarnessId::Cline
        // Qoder's ACP server accepts the same Stdio/SSE/HTTP MCP servers as
        // its CLI (see Qoder CLI docs, "ACP" → MCP Server).
        | HarnessId::Qoder
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
