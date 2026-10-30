//! Interaction-surface declarations (transparency plan Task 0).
//!
//! The point of these tests is that the table is **complete and explicit**: a
//! `HarnessId` with no row, or a row that quietly drifted from the reviewed
//! matrix, must fail here rather than surface as a missing affordance.

use komet_harness::capabilities::interaction_surface;
use komet_proto::{
    CompactionSurface, HarnessId, HarnessInteractionSurface, PlanSurface, ProgressSurface,
    QuestionSurface, SubagentSurface,
};

const ACP_TAGGED_IF_EMITTED: HarnessInteractionSurface = HarnessInteractionSurface {
    questions: QuestionSurface::PermissionShaped,
    // Gate B: the draft compaction signals are not advertised until an agent is
    // verified emitting them.
    compaction: CompactionSurface::None,
    subagents: SubagentSurface::TaggedIfEmitted,
    plans: PlanSurface::ToolChip,
    tool_progress: ProgressSurface::Streaming,
};

const ACP_NESTED_SESSION: HarnessInteractionSurface = HarnessInteractionSurface {
    subagents: SubagentSurface::NestedSession,
    ..ACP_TAGGED_IF_EMITTED
};

/// The golden matrix (plan §2), one row per `HarnessId`.
const GOLDEN: &[(HarnessId, HarnessInteractionSurface)] = &[
    (
        HarnessId::ClaudeCode,
        HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::LifecycleWithSummary,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::ToolChip,
            // No partial tool stdout from this CLI: declared, never simulated.
            tool_progress: ProgressSurface::None,
        },
    ),
    (
        HarnessId::Codex,
        HarnessInteractionSurface {
            questions: QuestionSurface::None,
            compaction: CompactionSurface::BoundaryOnly,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
    ),
    (
        HarnessId::Cursor,
        HarnessInteractionSurface {
            questions: QuestionSurface::None,
            compaction: CompactionSurface::BoundaryOnly,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::ToolChip,
            tool_progress: ProgressSurface::Final,
        },
    ),
    (HarnessId::Grok, ACP_NESTED_SESSION),
    (HarnessId::Opencode, ACP_NESTED_SESSION),
    (HarnessId::Devin, ACP_TAGGED_IF_EMITTED),
    (HarnessId::Copilot, ACP_TAGGED_IF_EMITTED),
    (HarnessId::Hermes, ACP_TAGGED_IF_EMITTED),
    (HarnessId::Pi, ACP_TAGGED_IF_EMITTED),
    (HarnessId::Cline, ACP_TAGGED_IF_EMITTED),
    (HarnessId::Qoder, ACP_TAGGED_IF_EMITTED),
    (
        HarnessId::Antigravity,
        HarnessInteractionSurface {
            // `agy -p` is one-shot: an in-run `ask_question` is surfaced as a
            // notice, never as an answerable panel (Task 6, Step 5).
            questions: QuestionSurface::None,
            compaction: CompactionSurface::None,
            subagents: SubagentSurface::None,
            plans: PlanSurface::ToolChip,
            tool_progress: ProgressSurface::Final,
        },
    ),
    (
        HarnessId::Muse,
        HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::Lifecycle,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
    ),
    (
        HarnessId::Mock,
        HarnessInteractionSurface {
            questions: QuestionSurface::Native,
            compaction: CompactionSurface::LifecycleWithSummary,
            subagents: SubagentSurface::Tagged,
            plans: PlanSurface::LivePanel,
            tool_progress: ProgressSurface::Streaming,
        },
    ),
];

#[test]
fn interaction_surfaces_are_declared() {
    for (id, expected) in GOLDEN {
        assert_eq!(
            interaction_surface(*id),
            *expected,
            "{}'s declared surface drifted from the matrix",
            id.as_str()
        );
    }
}

#[test]
fn every_harness_id_has_a_golden_row() {
    // The count is the trap: a new `HarnessId` (or a new row missing here) fails
    // before the registry can show a harness with no declared surface.
    let mut seen = std::collections::HashSet::new();
    for (id, _) in GOLDEN {
        assert!(
            seen.insert(id.as_str()),
            "duplicate golden row for {}",
            id.as_str()
        );
    }
    assert_eq!(
        seen.len(),
        14,
        "the matrix must carry one row per harness (Qoder included)"
    );
    for id in [
        HarnessId::ClaudeCode,
        HarnessId::Codex,
        HarnessId::Cursor,
        HarnessId::Devin,
        HarnessId::Grok,
        HarnessId::Hermes,
        HarnessId::Pi,
        HarnessId::Opencode,
        HarnessId::Cline,
        HarnessId::Qoder,
        HarnessId::Antigravity,
        HarnessId::Muse,
        HarnessId::Copilot,
        HarnessId::Mock,
    ] {
        assert!(
            seen.contains(id.as_str()),
            "{} has no golden row",
            id.as_str()
        );
    }
}

#[test]
fn no_harness_is_declared_as_an_all_zero_silent_surface() {
    // A row of all-`None`s is the silent no-op AGENTS.md forbids shipping: it
    // reads as "this agent shows nothing" instead of "nothing is mapped yet".
    for (id, _) in GOLDEN {
        let surface = interaction_surface(*id);
        let bare = surface.questions == QuestionSurface::None
            && surface.compaction == CompactionSurface::None
            && surface.subagents == SubagentSurface::None
            && surface.plans == PlanSurface::None
            && surface.tool_progress == ProgressSurface::None;
        assert!(
            !bare,
            "{} declares an empty surface — say what is absent, on purpose",
            id.as_str()
        );
    }
}

#[test]
fn plan_and_progress_surfaces_match_the_mapped_wires() {
    // The two columns Task 7 wrote: a harness whose adapter folds the live-plan
    // singleton is `LivePanel`, one whose partial tool output reaches
    // `ToolProgress` is `Streaming`. Both claims are code, so pin them here.
    for id in [HarnessId::Codex, HarnessId::Muse, HarnessId::Mock] {
        assert_eq!(
            interaction_surface(id).plans,
            PlanSurface::LivePanel,
            "{} should drive the persistent plan card",
            id.as_str()
        );
    }
    assert_eq!(
        interaction_surface(HarnessId::ClaudeCode).tool_progress,
        ProgressSurface::None,
        "Claude Code streams no partial tool output"
    );
}
