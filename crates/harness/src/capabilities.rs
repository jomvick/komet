use komet_proto::{
    AccountMode, CompactionSurface, HarnessCapabilities, HarnessId, HarnessInteractionSurface,
    IdentityCapability, IdentityDetection, PlanSurface, ProgressSurface, QuestionSurface,
    SubagentSurface, UsageCapability, UsageSurface,
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
            // No partial tool stdout on the 2.1.x wire — declared, never
            // simulated. What IS carried: a background Bash task's settled
            // `task_notification` (status + the CLI's own one-line summary) as
            // a final `ToolProgress` snapshot, which is `Final`, not
            // `Streaming`.
            tool_progress: ProgressSurface::Final,
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

/// Account, identity and real-usage decisions for every agent, as the
/// integrations actually stand. This is the Phase-0 matrix from
/// `fiabiliser les agents Komet`, and the single source of truth: the
/// descriptor, Settings and Usage read it instead of keeping parallel lists.
///
/// `accounts` is about Komet's ability to keep and switch logins, not about a
/// provider allowing several sessions. `identity.source` names the store/API
/// actually read. `usage.source` names the quota endpoint, local log or
/// measurement actually used; a source that is not declared here must not be
/// advertised by a surface.
pub fn capabilities(id: HarnessId) -> HarnessCapabilities {
    match id {
        HarnessId::ClaudeCode => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Claude config oauthAccount + Keychain credentials".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::ProviderQuota,
                source: "Anthropic OAuth usage endpoint".into(),
            },
        },
        HarnessId::Codex => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Codex auth.json account claims".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::ProviderQuota,
                source: "ChatGPT usage endpoint".into(),
            },
        },
        HarnessId::Cursor => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Cursor auth.json + state.vscdb profile".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::ProviderQuota,
                source: "Cursor usage endpoint".into(),
            },
        },
        HarnessId::Devin => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Devin CLI credential store".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::MeasuredOnly,
                source: "Komet-measured turns (no quota endpoint, no counter)".into(),
            },
        },
        HarnessId::Grok => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Grok CLI auth store".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::LocalLogs,
                source: "Grok CLI local usage log".into(),
            },
        },
        HarnessId::Hermes => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Hermes CLI auth store".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::MeasuredOnly,
                source: "Komet-measured turns (no quota endpoint, no counter)".into(),
            },
        },
        HarnessId::Pi => HarnessCapabilities {
            // One provider login at a time; Komet has no switch path.
            accounts: AccountMode::MonoAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::NotExposed,
                source: "Pi auth.json provider label (no account identity)".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::LocalLogs,
                source: "Pi local usage log".into(),
            },
        },
        HarnessId::Opencode => HarnessCapabilities {
            // CLI-managed single credential; adding replaces the active login.
            accounts: AccountMode::MonoAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "OpenCode SQLite credential table".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::LocalLogs,
                source: "OpenCode local usage log".into(),
            },
        },
        HarnessId::Cline => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Cline CLI credential store".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::MeasuredOnly,
                source: "Komet-measured turns (no quota endpoint, no counter)".into(),
            },
        },
        HarnessId::Qoder => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                // The login file is opaque bytes: Komet can hold and switch
                // several slots, but cannot name the person behind one.
                detection: IdentityDetection::NotExposed,
                source: "Qoder opaque credential file (no account identity)".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::MeasuredOnly,
                source: "Komet-measured turns (no quota endpoint, no counter)".into(),
            },
        },
        HarnessId::Copilot => HarnessCapabilities {
            // GitHub login is a single token slot: a new login replaces it.
            accounts: AccountMode::MonoAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "GitHub Copilot token / user endpoint".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::ProviderQuota,
                source: "GitHub Copilot quota endpoint".into(),
            },
        },
        HarnessId::Antigravity => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Antigravity keyring + Google userinfo (opaque fallback)".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::ProviderQuota,
                source: "Antigravity quota endpoint".into(),
            },
        },
        HarnessId::Muse => HarnessCapabilities {
            accounts: AccountMode::MultiAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::Detectable,
                source: "Muse CLI auth store".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::LocalLogs,
                source: "Muse local usage log".into(),
            },
        },
        HarnessId::Mock => HarnessCapabilities {
            accounts: AccountMode::MonoAccount,
            identity: IdentityCapability {
                detection: IdentityDetection::NotExposed,
                source: "test double".into(),
            },
            usage: UsageCapability {
                surface: UsageSurface::MeasuredOnly,
                source: "test double".into(),
            },
        },
    }
}

/// Quota-capable agents, derived from the matrix (never a second list).
pub fn reports_provider_quota(id: HarnessId) -> bool {
    capabilities(id).usage.surface == UsageSurface::ProviderQuota
}

/// Agents whose usage comes from a readable CLI log.
pub fn has_local_usage_log(id: HarnessId) -> bool {
    capabilities(id).usage.surface == UsageSurface::LocalLogs
}

/// Agents Komet can switch between saved logins.
pub fn supports_account_switching(id: HarnessId) -> bool {
    capabilities(id).accounts == AccountMode::MultiAccount
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

/// True where komet can mediate the agent's NATIVE `mcpServers` config at
/// launch — applying per-agent overlay enable/disable decisions without
/// writing the agent's files:
/// - Claude Code: `--strict-mcp-config` + merged `--mcp-config` document;
/// - OpenCode: `OPENCODE_CONFIG_CONTENT` disable list;
/// - Codex: `-c mcp_servers.<name>.enabled=<bool>` per-run overrides.
///
/// Returns `(supported, reason)`; `reason` is a human-readable explanation
/// (French) for UI when unsupported. Agents without a mediation path must
/// surface read-only extension rows rather than silent no-op toggles.
pub fn mediates_native_mcp(id: HarnessId) -> (bool, &'static str) {
    match id {
        HarnessId::ClaudeCode | HarnessId::Opencode | HarnessId::Codex => (true, ""),
        _ => (
            false,
            "komet ne peut pas modifier la config MCP native de cet agent",
        ),
    }
}
