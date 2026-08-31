//! Model catalog + effort mapping for Claude Code, ported from komet's
//! `packages/harness/src/claude.ts` (which itself mirrors Claude Code's own
//! picker via t3code's catalog).
//!
//! The TS harness discovers models at runtime through the SDK's
//! `supportedModels()` control request and then OVERLAYS these static effort
//! ladders / option sets (the SDK under-reports both). Until we grow a
//! short-lived control-channel discovery session, [`static_models`] returns the
//! curated list directly; `ClaudeHarness::models` is the single seam where
//! dynamic discovery can later be spliced in.

use komet_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel, SlashCommand};

/// Curated list of Claude Code built-in slash commands (Bucket A) and bundled skills.
pub(crate) fn static_commands() -> Vec<SlashCommand> {
    vec![
        // Context / Session Management
        SlashCommand {
            name: "compact".into(),
            description: "Compact conversation history to free context space".into(),
            input_hint: Some("[instructions]".into()),
        },
        SlashCommand {
            name: "context".into(),
            description: "Show token usage and context breakdown".into(),
            input_hint: Some("[all]".into()),
        },
        SlashCommand {
            name: "cost".into(),
            description: "Show token and cost usage statistics".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "clear".into(),
            description: "Reset conversation history and start fresh".into(),
            input_hint: Some("[name]".into()),
        },
        SlashCommand {
            name: "rewind".into(),
            description: "Rewind files to a previous checkpoint".into(),
            input_hint: Some("[checkpoint]".into()),
        },
        SlashCommand {
            name: "diff".into(),
            description: "Show uncommitted changes in the repository".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "branch".into(),
            description: "Create or switch to a git branch".into(),
            input_hint: Some("<branch name>".into()),
        },
        SlashCommand {
            name: "rename".into(),
            description: "Rename the current session".into(),
            input_hint: Some("<new name>".into()),
        },
        SlashCommand {
            name: "export".into(),
            description: "Export conversation history to a file".into(),
            input_hint: Some("<filename>".into()),
        },
        SlashCommand {
            name: "plan".into(),
            description: "Switch to planning mode with a goal description".into(),
            input_hint: Some("<description>".into()),
        },
        SlashCommand {
            name: "goal".into(),
            description: "Set a high-level goal condition or clear it".into(),
            input_hint: Some("<condition|clear>".into()),
        },
        SlashCommand {
            name: "btw".into(),
            description: "Ask a quick side-question without polluting primary context".into(),
            input_hint: Some("<question>".into()),
        },
        SlashCommand {
            name: "add-dir".into(),
            description: "Add a directory to the context workspace".into(),
            input_hint: Some("<path>".into()),
        },
        SlashCommand {
            name: "cd".into(),
            description: "Change working directory".into(),
            input_hint: Some("<path>".into()),
        },
        SlashCommand {
            name: "subtask".into(),
            description: "Run a subagent task in the current session".into(),
            input_hint: Some("<task description>".into()),
        },
        SlashCommand {
            name: "config".into(),
            description: "View or modify a configuration setting".into(),
            input_hint: Some("<key=value>".into()),
        },
        SlashCommand {
            name: "memory".into(),
            description: "View or edit CLAUDE.md memory file".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "sandbox".into(),
            description: "View or adjust execution sandbox policy".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "init".into(),
            description: "Initialize CLAUDE.md memory file for current project".into(),
            input_hint: None,
        },
        // Bundled skills
        SlashCommand {
            name: "review".into(),
            description: "Review changes or a pull request".into(),
            input_hint: Some("[pr number]".into()),
        },
        SlashCommand {
            name: "security-review".into(),
            description: "Run a comprehensive security review".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "simplify".into(),
            description: "Simplify code for clarity and maintainability".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "loop".into(),
            description: "Run an autonomous loop until completion".into(),
            input_hint: Some("<prompt>".into()),
        },
        SlashCommand {
            name: "claude-api".into(),
            description: "Search Claude API reference and examples".into(),
            input_hint: Some("<query>".into()),
        },
        SlashCommand {
            name: "dataviz".into(),
            description: "Generate charts and data visualizations".into(),
            input_hint: Some("<data/request>".into()),
        },
        SlashCommand {
            name: "run".into(),
            description: "Run a script or command".into(),
            input_hint: Some("<command>".into()),
        },
        SlashCommand {
            name: "batch".into(),
            description: "Execute a batch of refactoring tasks".into(),
            input_hint: Some("<tasks>".into()),
        },
        SlashCommand {
            name: "doctor".into(),
            description: "Diagnose common project configuration issues".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "debug".into(),
            description: "Diagnose and debug errors or failing tests".into(),
            input_hint: Some("<issue>".into()),
        },
        SlashCommand {
            name: "verify".into(),
            description: "Verify test results and build integrity".into(),
            input_hint: None,
        },
    ]
}

/// The ultrathink directive rides every user message as a prompt prefix — that
/// is how the mode actually works in Claude Code (a prompt convention, not an
/// effort flag). Applied to the initial prompt AND every steer.
pub(crate) const ULTRATHINK_PREFIX: &str = "Ultrathink:\n";

pub(crate) fn apply_ultrathink(reasoning: Option<ReasoningLevel>, text: &str) -> String {
    if reasoning == Some(ReasoningLevel::Ultrathink) {
        format!("{ULTRATHINK_PREFIX}{text}")
    } else {
        text.to_owned()
    }
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Models whose CLI accepts `xhigh` natively; elsewhere it clamps to `max`
/// (mirroring Claude Code's own normalization). Substring port of claude.ts's
/// `/fable-5|opus-4-[7-9]|opus-[5-9]|sonnet-[5-9]/`.
pub(crate) fn supports_xhigh(model: &str) -> bool {
    contains_any(
        model,
        &[
            "fable-5", "opus-4-7", "opus-4-8", "opus-4-9", "opus-5", "opus-6", "opus-7", "opus-8",
            "opus-9", "sonnet-5", "sonnet-6", "sonnet-7", "sonnet-8", "sonnet-9",
        ],
    )
}

/// Map the unified level to the `--effort` flag value the CLI accepts for this
/// model. The special modes don't translate directly: `ultrathink` is a prompt
/// prefix (no flag), `ultracode` runs as `xhigh` plus the ultracode setting,
/// and `ultra` is a Codex-only tier (Claude tops out at `max`).
pub(crate) fn to_effort(
    reasoning: Option<ReasoningLevel>,
    model: Option<&str>,
) -> Option<&'static str> {
    let base = match reasoning? {
        ReasoningLevel::Ultrathink => return None,
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode => "xhigh",
        ReasoningLevel::Max | ReasoningLevel::Ultra => "max",
    };
    if base == "xhigh" && !model.is_some_and(supports_xhigh) {
        return Some("max");
    }
    Some(base)
}

/// A boolean toggle rendered as an off/on select (the Rust `ModelOption` wire
/// type has no dedicated boolean kind).
fn toggle(id: &str, label: &str) -> ModelOption {
    ModelOption {
        id: id.into(),
        label: label.into(),
        choices: vec![
            ModelOptionChoice {
                id: "off".into(),
                label: "Off".into(),
            },
            ModelOptionChoice {
                id: "on".into(),
                label: "On".into(),
            },
        ],
        default_choice: "off".into(),
    }
}

/// The 200K/1M context-window select carried by the long-context models. The
/// 1M window is selected via a model-id suffix (`<model>[1m]`), exactly how the
/// CLI itself does it.
pub(crate) fn context_window() -> ModelOption {
    ModelOption {
        id: "contextWindow".into(),
        label: "Context Window".into(),
        choices: vec![
            ModelOptionChoice {
                id: "200k".into(),
                label: "200K".into(),
            },
            ModelOptionChoice {
                id: "1m".into(),
                label: "1M".into(),
            },
        ],
        default_choice: "200k".into(),
    }
}

const FULL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultracode,
    ReasoningLevel::Ultrathink,
];

/// opus-4-7 / sonnet-5+ tier (claude.ts `claudeEffortsFor`): xhigh native,
/// no ultracode.
const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultrathink,
];

fn model(
    id: &str,
    label: &str,
    description: &str,
    ladder: &[ReasoningLevel],
    options: Vec<ModelOption>,
) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options,
    }
}

/// The curated model list, mirroring claude.ts's `claudeEffortsFor` /
/// `claudeOptionsFor` ladders: full ladder (through ultracode/ultrathink) on
/// Fable 5, `max`-topped ladders on Opus/Sonnet, no efforts but a thinking
/// toggle on Haiku; context-window select on the long-context families and
/// fast mode on Opus 4.5+.
///
/// `pub`: besides the discovery-side enrichment here, the UI's display-side
/// normalization borrows these labels so alias rows served by older engines
/// still read with their version numbers ("Opus 5", not "Opus").
pub fn static_models() -> Vec<Model> {
    vec![
        model(
            "claude-fable-5",
            "Fable 5",
            "Most intelligent model for building agents",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-opus-5",
            "Opus 5",
            "Powerful model for complex work",
            FULL_LADDER,
            vec![context_window(), toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-opus-4-8",
            "Opus 4.8",
            "Previous generation Opus",
            FULL_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-opus-4-7",
            "Opus 4.7",
            "Older generation Opus",
            XHIGH_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-sonnet-5",
            "Sonnet 5",
            "Balanced speed and intelligence",
            XHIGH_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-haiku-4-5",
            "Haiku 4.5",
            "Fastest model for everyday tasks",
            &[],
            vec![toggle("thinking", "Thinking")],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_maps_special_modes() {
        assert_eq!(to_effort(None, None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Ultrathink), None), None);
        assert_eq!(
            to_effort(Some(ReasoningLevel::Minimal), Some("claude-fable-5")),
            Some("low")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultra), Some("claude-fable-5")),
            Some("max")
        );
        // ultracode -> xhigh where supported…
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultracode), Some("claude-fable-5")),
            Some("xhigh")
        );
        // …and xhigh clamps to max elsewhere.
        assert_eq!(
            to_effort(Some(ReasoningLevel::XHigh), Some("claude-opus-4-5")),
            Some("max")
        );
        assert_eq!(to_effort(Some(ReasoningLevel::XHigh), None), Some("max"));
    }

    #[test]
    fn xhigh_family_matching() {
        assert!(supports_xhigh("claude-fable-5"));
        assert!(supports_xhigh("claude-opus-5"));
        assert!(supports_xhigh("claude-opus-5[1m]"));
        assert!(supports_xhigh("claude-opus-4-7-20260101"));
        assert!(!supports_xhigh("claude-opus-4-5"));
        assert!(!supports_xhigh("claude-sonnet-4-5"));
    }

    #[test]
    fn ultrathink_prefixes_prompt() {
        assert_eq!(
            apply_ultrathink(Some(ReasoningLevel::Ultrathink), "do it"),
            "Ultrathink:\ndo it"
        );
        assert_eq!(
            apply_ultrathink(Some(ReasoningLevel::Max), "do it"),
            "do it"
        );
    }
}
