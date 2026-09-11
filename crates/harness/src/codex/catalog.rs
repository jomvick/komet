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
fn wire_effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    }
}

/// Map the unified level to the wire `effort` field. When `model` is a known
/// catalog id, unsupported leftovers (e.g. Medium on a high-only model) are
/// omitted — Codex rejects those the same way `agy` rejects `--effort` on Claude.
pub(crate) fn to_effort(
    reasoning: Option<ReasoningLevel>,
    model: Option<&str>,
    catalog: &[Model],
) -> Option<&'static str> {
    let mapped = wire_effort(reasoning?);
    if let Some(id) = model.filter(|id| !id.is_empty())
        && let Some(entry) = catalog.iter().find(|m| m.id == id)
    {
        let offered: Vec<&str> = entry
            .reasoning_levels
            .iter()
            .copied()
            .map(wire_effort)
            .collect();
        if !offered.contains(&mapped) {
            return None;
        }
    }
    Some(mapped)
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

fn model(id: &str, label: &str, description: &str, ladder: Vec<ReasoningLevel>) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder,
        options: vec![service_tier()],
    }
}

/// Map a live `models_cache.json` effort list to the exact levels Codex
/// advertises — not the nearest full ladder. A high-only model (`gpt-5.5`)
/// must not offer Medium/XHigh or the picker will send a rejected `effort`.
fn ladder_for_efforts(efforts: &[String]) -> Vec<ReasoningLevel> {
    let mut out = Vec::new();
    for effort in efforts {
        let level = match effort.as_str() {
            "minimal" => ReasoningLevel::Minimal,
            "low" => ReasoningLevel::Low,
            "medium" => ReasoningLevel::Medium,
            "high" => ReasoningLevel::High,
            "xhigh" => ReasoningLevel::XHigh,
            "max" => ReasoningLevel::Max,
            "ultra" => ReasoningLevel::Ultra,
            _ => continue,
        };
        if !out.contains(&level) {
            out.push(level);
        }
    }
    out
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

/// Live `~/.codex/models_cache.json` first, curated snapshot as fallback.
/// Shared by [`CodexHarness::models`] and [`to_effort`] so the picker and the
/// wire never disagree on which efforts a slug accepts.
pub(crate) fn load_catalog() -> Vec<Model> {
    if let Some(home) = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")))
    {
        let cache = home.join("models_cache.json");
        if let Ok(text) = std::fs::read_to_string(&cache)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(models) = parse_models_cache(&value)
        {
            return models;
        }
    }
    static_models()
}

/// The curated catalog: a snapshot of the live `models_cache.json` (codex-cli
/// 0.147) — keep in sync; stale ids are rejected by the app server. Mirrors
/// codex.ts's `CODEX_MODELS` fallback. Ladders match `supported_reasoning_levels`.
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            "Balanced agentic coding model for everyday work.",
            vec![ReasoningLevel::Low, ReasoningLevel::Ultra],
        ),
        model(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            "Fast and affordable agentic coding model.",
            vec![ReasoningLevel::Low, ReasoningLevel::Max],
        ),
        model(
            "gpt-5.5",
            "GPT-5.5",
            "Frontier model for complex coding, research, and real-world work.",
            vec![ReasoningLevel::High],
        ),
        model(
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "Small, fast, and cost-efficient model for simpler coding tasks.",
            vec![ReasoningLevel::High],
        ),
        model(
            "codex-auto-review",
            "Codex Auto Review",
            "Automatic approval review model for Codex.",
            vec![ReasoningLevel::Max],
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
        assert_eq!(to_effort(None, None, &static_models()), None);
        assert_eq!(
            to_effort(Some(ReasoningLevel::Minimal), None, &static_models()),
            Some("low")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultracode), None, &static_models()),
            Some("xhigh")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultrathink), None, &static_models()),
            Some("xhigh")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Max), None, &static_models()),
            Some("max")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultra), None, &static_models()),
            Some("ultra")
        );
        // Known high-only model: leftover Medium must not go on the wire.
        assert_eq!(
            to_effort(
                Some(ReasoningLevel::Medium),
                Some("gpt-5.5"),
                &static_models()
            ),
            None
        );
        assert_eq!(
            to_effort(
                Some(ReasoningLevel::High),
                Some("gpt-5.5"),
                &static_models()
            ),
            Some("high")
        );
        // Unknown slug: still forward (the server is the source of truth).
        assert_eq!(
            to_effort(
                Some(ReasoningLevel::Ultra),
                Some("gpt-5.6-sol"),
                &static_models()
            ),
            Some("ultra")
        );
    }

    #[test]
    fn catalog_is_newest_first_with_service_tiers() {
        let models = static_models();
        assert_eq!(models.len(), 5);
        assert_eq!(models[0].id, "gpt-5.6-terra");
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        assert!(!models[0].reasoning_levels.contains(&ReasoningLevel::High));
        assert_eq!(models[2].reasoning_levels, vec![ReasoningLevel::High]);
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
        assert_eq!(
            parsed[0].reasoning_levels,
            vec![ReasoningLevel::Low, ReasoningLevel::Ultra]
        );
    }

    #[test]
    fn parse_models_cache_keeps_single_effort_ladders_exact() {
        let value = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "supported_reasoning_levels": [{"effort": "high"}]
                }
            ]
        });
        let parsed = parse_models_cache(&value).expect("parses");
        assert_eq!(parsed[0].reasoning_levels, vec![ReasoningLevel::High]);
    }
}
