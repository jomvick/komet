//! Model catalog + effort mapping for Codex, ported from komet's
//! `packages/harness/src/codex.ts`.
//!
//! The TS harness discovers models live via the app server's `model/list`
//! (experimentalApi) and falls back to a curated snapshot; here the snapshot IS
//! the catalog, and `CodexHarness::models` is the single seam where a
//! short-lived `codex app-server` + `model/list` pagination can later be
//! spliced in (same call t3code's Codex provider makes).

use komet_proto::{
    Model, ModelOption, ModelOptionChoice, ReasoningLevel, SandboxLevel, SlashCommand,
};

/// The unified reasoning ladder Codex accepts (`minimal` is offered but clamped
/// on the wire — see [`to_effort`]).
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

/// Codex's API rejects `minimal` when default tools (web_search, image_gen)
/// are enabled, and doesn't know Claude's ultracode/ultrathink modes. It DOES
/// accept `max` and `ultra` natively (gpt-5.6+), so those pass straight
/// through — only the levels Codex can't take are clamped to the nearest
/// effort (port of codex.ts `toEffort`).
pub(crate) fn to_effort(reasoning: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match reasoning? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    })
}

/// `thread/start`'s `sandbox` param (kebab-case wire words).
#[allow(dead_code)]
pub(crate) fn sandbox_mode(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace-write",
        SandboxLevel::DangerFullAccess => "danger-full-access",
    }
}

/// `turn/start`'s `sandboxPolicy.type` (camelCase variant of the same policy).
#[allow(dead_code)]
pub(crate) fn sandbox_policy_type(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "readOnly",
        SandboxLevel::WorkspaceWrite => "workspaceWrite",
        SandboxLevel::DangerFullAccess => "dangerFullAccess",
    }
}

/// `turn/start`'s full `sandboxPolicy` object. Workspace-write keeps network
/// access: komet agents fetch deps and hit APIs unattended, and with the
/// approval policy pinned to "never" a network-less sandbox would fail those
/// commands with no escalation path.
#[allow(dead_code)]
pub(crate) fn sandbox_policy_value(sandbox: SandboxLevel) -> serde_json::Value {
    let mut policy = serde_json::Map::new();
    policy.insert("type".into(), sandbox_policy_type(sandbox).into());
    if matches!(sandbox, SandboxLevel::WorkspaceWrite) {
        policy.insert("networkAccess".into(), true.into());
    }
    serde_json::Value::Object(policy)
}

const ULTRA_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

const MAX_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

/// The service-tier select the app server reports per model (`serviceTiers` /
/// `additionalSpeedTiers` in `model/list`); "default" means Standard and is
/// omitted from the wire params entirely.
fn service_tier() -> ModelOption {
    ModelOption {
        id: "serviceTier".into(),
        label: "Service Tier".into(),
        choices: vec![
            ModelOptionChoice {
                id: "default".into(),
                label: "Standard".into(),
            },
            ModelOptionChoice {
                id: "fast".into(),
                label: "Fast".into(),
            },
        ],
        default_choice: "default".into(),
    }
}

fn model(id: &str, label: &str, description: &str, ladder: &[ReasoningLevel]) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options: vec![service_tier()],
    }
}

/// Map a live `models_cache.json` effort list to the closest ladder.
fn ladder_for_efforts(efforts: &[String]) -> &'static [ReasoningLevel] {
    if efforts.iter().any(|e| e == "ultra") {
        ULTRA_LADDER
    } else if efforts.iter().any(|e| e == "max") {
        MAX_LADDER
    } else {
        XHIGH_LADDER
    }
}

/// Parse a `~/.codex/models_cache.json` document into models. `None` when the
/// cache has no usable entries (caller falls back to [`static_models`]).
pub(crate) fn parse_models_cache(value: &serde_json::Value) -> Option<Vec<Model>> {
    let models = value.get("models")?.as_array()?;
    let mut out = Vec::new();
    for m in models {
        let id = m.get("slug")?.as_str()?;
        if m.get("visibility")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| v == "hidden")
        {
            continue;
        }
        let label = m
            .get("display_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id);
        let description = m
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let efforts: Vec<String> = m
            .get("supported_reasoning_levels")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("effort")?.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        out.push(model(id, label, description, ladder_for_efforts(&efforts)));
    }
    (!out.is_empty()).then_some(out)
}

/// The curated catalog: a snapshot of the live `models_cache.json` (codex-cli
/// 0.147) — keep in sync; stale ids are rejected by the app server. Mirrors
/// codex.ts's `CODEX_MODELS` fallback.
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            "Balanced agentic coding model for everyday work.",
            ULTRA_LADDER,
        ),
        model(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            "Fast and affordable agentic coding model.",
            MAX_LADDER,
        ),
        model(
            "gpt-5.5",
            "GPT-5.5",
            "Frontier model for complex coding, research, and real-world work.",
            XHIGH_LADDER,
        ),
        model(
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "Small, fast, and cost-efficient model for simpler coding tasks.",
            XHIGH_LADDER,
        ),
        model(
            "codex-auto-review",
            "Codex Auto Review",
            "Automatic approval review model for Codex.",
            MAX_LADDER,
        ),
    ]
}

/// Built-in slash commands native to Codex CLI / TUI.
pub(crate) fn static_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "compact".into(),
            description: "Compact the session context window".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "diff".into(),
            description: "View current workspace git diff".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "clear".into(),
            description: "Clear context and start a new thread".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "undo".into(),
            description: "Undo the last tool/file modifications".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "review".into(),
            description: "Review current changes or a git commit".into(),
            input_hint: Some("[commit|branch]".into()),
        },
        SlashCommand {
            name: "model".into(),
            description: "Switch active model or reasoning effort".into(),
            input_hint: Some("[model_id]".into()),
        },
        SlashCommand {
            name: "plan".into(),
            description: "Create or iterate on an implementation plan".into(),
            input_hint: Some("[prompt]".into()),
        },
        SlashCommand {
            name: "export".into(),
            description: "Export current conversation transcript".into(),
            input_hint: Some("[format]".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_clamps_like_codex_ts() {
        assert_eq!(to_effort(None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Minimal)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultracode)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultrathink)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Max)), Some("max"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultra)), Some("ultra"));
    }

    #[test]
    fn catalog_is_newest_first_with_service_tiers() {
        let models = static_models();
        assert_eq!(models.len(), 5);
        assert_eq!(models[0].id, "gpt-5.6-terra");
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        assert!(!models[2].reasoning_levels.contains(&ReasoningLevel::Max));
        for m in &models {
            let tier = m.options.iter().find(|o| o.id == "serviceTier");
            assert!(tier.is_some(), "{} missing serviceTier", m.id);
        }
    }

    #[test]
    fn parse_models_cache_reads_live_cache() {
        let value = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-terra",
                    "display_name": "GPT-5.6-Terra",
                    "description": "Balanced agentic coding model.",
                    "visibility": "list",
                    "supported_reasoning_levels": [
                        {"effort": "low"}, {"effort": "ultra"}
                    ]
                },
                {
                    "slug": "hidden-model",
                    "display_name": "Hidden",
                    "visibility": "hidden",
                    "supported_reasoning_levels": [{"effort": "low"}]
                }
            ]
        });
        let parsed = parse_models_cache(&value).expect("parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "gpt-5.6-terra");
        assert!(parsed[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
    }
}
