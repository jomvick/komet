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
///
/// The CLI's own `list_models` entries use FAMILY ALIASES as ids (`default`,
/// `opus[1m]`, `sonnet`, `haiku`) and advertise `supportedEffortLevels`
/// including `xhigh` for them, so those alias shapes are recognized too —
/// exactly, not by substring, which would drag `opus-4-5` back in.
pub(crate) fn supports_xhigh(model: &str) -> bool {
    let alias = model.split('[').next().unwrap_or(model);
    matches!(alias, "default" | "opus" | "sonnet")
        || contains_any(
            model,
            &[
                "fable-5",
                "opus-4-7",
                "opus-4-8",
                "opus-4-9",
                "opus-5",
                "opus-6",
                "opus-7",
                "opus-8",
                "opus-9",
                "sonnet-5",
                "sonnet-6",
                "sonnet-7",
                "sonnet-8",
                "sonnet-9",
                "3-7-sonnet",
            ],
        )
}

/// Map the unified level to the `--effort` flag value the CLI accepts for this
/// model. The special modes don't translate directly: `ultrathink` is a prompt
/// prefix (no flag), `ultracode` runs as `xhigh` plus the ultracode setting,
/// and `ultra` is a Codex-only tier (Claude tops out at `max`).
///
/// Haiku (and any catalog model with an empty ladder) rejects `--effort`;
/// thinking is the `thinking` toggle, not an effort flag.
pub(crate) fn to_effort(
    reasoning: Option<ReasoningLevel>,
    model: Option<&str>,
) -> Option<&'static str> {
    if model.is_some_and(rejects_effort) {
        return None;
    }
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

/// Haiku has no `--effort` ladder — hybrid thinking is the `thinking` toggle.
fn rejects_effort(model: &str) -> bool {
    model.contains("haiku")
        || static_models()
            .iter()
            .any(|m| m.id == model && m.reasoning_levels.is_empty())
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

/// Read models configured in Claude CLI settings files (~/.claude/settings.json, ~/.claude/settings.local.json)
/// and environment variables (ANTHROPIC_MODEL, etc.), mirroring Paseo's `getClaudeModelsWithSettings`.
pub fn read_claude_settings_models() -> Vec<Model> {
    let mut custom = Vec::new();

    // Check ANTHROPIC_MODEL env var
    if let Ok(env_model) = std::env::var("ANTHROPIC_MODEL") {
        let trimmed = env_model.trim();
        if !trimmed.is_empty() {
            custom.push(model(
                trimmed,
                trimmed,
                "From environment ANTHROPIC_MODEL",
                FULL_LADDER,
                vec![context_window()],
            ));
        }
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);

    if let Some(home) = home {
        let claude_dir = home.join(".claude");
        for settings_name in ["settings.json", "settings.local.json"] {
            let path = claude_dir.join(settings_name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(model_str) = val.get("model").and_then(|v| v.as_str()) {
                        let m = model_str.trim();
                        if !m.is_empty() && !custom.iter().any(|c| c.id == m) {
                            custom.push(model(
                                m,
                                m,
                                &format!("Configured in ~/.claude/{settings_name}"),
                                FULL_LADDER,
                                vec![context_window()],
                            ));
                        }
                    }
                    if let Some(env_obj) = val.get("env").and_then(|v| v.as_object()) {
                        for (k, v) in env_obj {
                            if (k.contains("MODEL") || k == "model")
                                && let Some(m) = v.as_str()
                            {
                                let m = m.trim();
                                if !m.is_empty() && !custom.iter().any(|c| c.id == m) {
                                    custom.push(model(
                                        m,
                                        m,
                                        &format!("From ~/.claude/{settings_name} env.{k}"),
                                        FULL_LADDER,
                                        vec![context_window()],
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    custom
}

/// The curated model list, prioritizing real official Claude models and user settings models.
pub fn static_models() -> Vec<Model> {
    let mut list = Vec::new();

    // 1. Primary flagship models (with alias compatibility labels for ACP)
    let primary = [
        model(
            "claude-opus-5",
            "Opus 5",
            "Powerful model for complex work",
            FULL_LADDER,
            vec![context_window(), toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-fable-5",
            "Fable 5",
            "Most intelligent model for building agents",
            FULL_LADDER,
            vec![context_window()],
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
        // Real-world Claude 3.7 / 3.5 / 3 models
        model(
            "claude-3-7-sonnet",
            "Claude 3.7 Sonnet",
            "Most intelligent hybrid reasoning model",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-3-5-sonnet",
            "Claude 3.5 Sonnet",
            "Industry-leading intelligence and coding performance",
            XHIGH_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-3-5-haiku",
            "Claude 3.5 Haiku",
            "Fastest and most responsive model",
            &[],
            vec![toggle("thinking", "Thinking")],
        ),
        model(
            "claude-3-opus",
            "Claude 3 Opus",
            "Powerful model for complex analysis",
            XHIGH_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        // Versioned snapshots
        model(
            "claude-3-7-sonnet-20250219",
            "Claude 3.7 Sonnet (2025-02-19)",
            "Snapshot of Claude 3.7 Sonnet",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-3-5-sonnet-20241022",
            "Claude 3.5 Sonnet (2024-10-22)",
            "Snapshot of Claude 3.5 Sonnet v2",
            XHIGH_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-3-5-haiku-20241022",
            "Claude 3.5 Haiku (2024-10-22)",
            "Snapshot of Claude 3.5 Haiku",
            &[],
            vec![toggle("thinking", "Thinking")],
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
    ];

    for m in primary {
        list.push(m);
    }

    // 2. User-configured custom models from settings and env
    for custom in read_claude_settings_models() {
        if !list.iter().any(|m: &Model| m.id == custom.id) {
            list.insert(0, custom);
        }
    }

    list
}

/// The CLI's own effort ladder for a live entry (`supportedEffortLevels`),
/// mapped onto the picker's levels. `None` when the wire says nothing, so the
/// curated family heuristics below still decide.
fn wire_ladder(item: &serde_json::Value) -> Option<Vec<ReasoningLevel>> {
    let names: Vec<String> = item
        .get("supportedEffortLevels")?
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect();
    if names.is_empty() {
        return None;
    }
    let has = |name: &str| names.iter().any(|n| n == name);
    let mut ladder = Vec::new();
    if has("minimal") {
        ladder.push(ReasoningLevel::Minimal);
    }
    if has("low") {
        ladder.push(ReasoningLevel::Low);
    }
    if has("medium") {
        ladder.push(ReasoningLevel::Medium);
    }
    if has("high") {
        ladder.push(ReasoningLevel::High);
    }
    if has("xhigh") {
        ladder.push(ReasoningLevel::XHigh);
    }
    if has("max") {
        ladder.push(ReasoningLevel::Max);
    }
    // komet-only modes ride the advertised levels instead of a new flag:
    // Ultracode is the CLI's `xhigh`, Ultrathink a prompt prefix.
    if has("xhigh") {
        ladder.push(ReasoningLevel::Ultracode);
    }
    ladder.push(ReasoningLevel::Ultrathink);
    Some(ladder)
}

/// Parse dynamically discovered models from a Claude control-channel response
/// (`list_models` on 2.1.x, `supported_models` on older builds; the shapes
/// differ only in which keys are present). Overlays curated effort ladders and
/// model options where the wire is silent.
pub(crate) fn parse_discovered_models(value: &serde_json::Value) -> Vec<Model> {
    let raw_list = if let Some(arr) = value.as_array() {
        arr.as_slice()
    } else if let Some(arr) = value.get("models").and_then(serde_json::Value::as_array) {
        arr.as_slice()
    } else if let Some(arr) = value
        .get("response")
        .and_then(|r| r.get("models"))
        .and_then(serde_json::Value::as_array)
    {
        arr.as_slice()
    } else {
        &[]
    };

    if raw_list.is_empty() {
        return Vec::new();
    }

    let static_list = static_models();
    let mut out = Vec::new();
    for item in raw_list {
        let id = item
            .get("id")
            .or_else(|| item.get("value"))
            .or_else(|| item.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(id) = id else { continue };

        if out.iter().any(|m: &Model| m.id == id) {
            continue;
        }

        let label = item
            .get("name")
            .or_else(|| item.get("displayName"))
            .or_else(|| item.get("display_name"))
            .or_else(|| item.get("label"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id);

        let description = item
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        // Overlay with curated static catalog if known
        if let Some(matched) = static_list.iter().find(|m| m.id == id) {
            out.push(Model {
                id: id.to_owned(),
                label: if label != id {
                    label.to_owned()
                } else {
                    matched.label.clone()
                },
                description: if !description.is_empty() {
                    Some(description.to_owned())
                } else {
                    matched.description.clone()
                },
                reasoning_levels: matched.reasoning_levels.clone(),
                options: matched.options.clone(),
            });
        } else {
            // Live/unrecognized model: prefer what the CLI itself advertises
            // (`supportedEffortLevels`, `supportsFastMode`) and keep the
            // curated family heuristics only for what the wire omits.
            let ladder = wire_ladder(item).unwrap_or_else(|| {
                if id.contains("haiku") {
                    Vec::new()
                } else if id.contains("opus-4-7") || id.contains("sonnet-5") {
                    XHIGH_LADDER.to_vec()
                } else {
                    FULL_LADDER.to_vec()
                }
            });

            // `[1m]` in the advertised id or its resolved model means this row
            // IS the long-context variant: a 200K/1M select on top of it would
            // be a lie (and would double the suffix on the wire).
            let already_one_m = ["value", "id", "resolvedModel", "model"]
                .iter()
                .filter_map(|key| item.get(key).and_then(serde_json::Value::as_str))
                .any(|v| v.contains("[1m]"));
            let mut options = Vec::new();
            if id.contains("haiku") {
                options.push(toggle("thinking", "Thinking"));
            } else if !already_one_m {
                options.push(context_window());
            }
            let fast_mode = match item
                .get("supportsFastMode")
                .and_then(serde_json::Value::as_bool)
            {
                Some(advertised) => advertised,
                None => id.contains("opus"),
            };
            if fast_mode {
                options.push(toggle("fastMode", "Fast Mode"));
            }

            out.push(model(id, label, description, &ladder, options));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_discovered_models_overlays_static_ladders() {
        let payload = serde_json::json!({
            "models": [
                { "id": "claude-opus-5", "name": "Opus 5 (Live)" },
                { "id": "claude-haiku-4-5", "name": "Haiku 4.5" },
                { "id": "claude-custom-new", "name": "Custom Model" }
            ]
        });
        let models = parse_discovered_models(&payload);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "claude-opus-5");
        assert_eq!(models[0].label, "Opus 5 (Live)");
        assert_eq!(models[0].reasoning_levels, FULL_LADDER);
        assert_eq!(models[1].id, "claude-haiku-4-5");
        assert!(models[1].reasoning_levels.is_empty());
        assert_eq!(models[2].id, "claude-custom-new");
        assert_eq!(models[2].reasoning_levels, FULL_LADDER);
    }

    /// The live shape of CLI 2.1.x `list_models` (captured verbatim from
    /// `claude --print … | list_models`): alias ids, `displayName`, and the
    /// CLI's own `supportedEffortLevels` / `supportsFastMode`.
    fn live_list_models_payload() -> serde_json::Value {
        serde_json::json!({
            "models": [
                {
                    "value": "default",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "displayName": "Default (recommended)",
                    "description": "Use the default model (currently Opus 4.8 (1M context)) · $5/$25 per Mtok",
                    "supportsEffort": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                    "supportsAdaptiveThinking": true,
                    "supportsFastMode": true,
                    "supportsAutoMode": true
                },
                {
                    "value": "opus[1m]",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "displayName": "Opus",
                    "description": "Opus 4.8 with 1M context · Best for everyday, complex tasks · $5/$25 per Mtok",
                    "supportsEffort": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                    "supportsFastMode": true
                },
                {
                    "value": "sonnet",
                    "resolvedModel": "claude-sonnet-5",
                    "displayName": "Sonnet",
                    "description": "Sonnet 5 · Efficient for routine tasks · $3/$15 per Mtok",
                    "supportsEffort": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]
                },
                {
                    "value": "haiku",
                    "resolvedModel": "claude-haiku-4-5-20251001",
                    "displayName": "Haiku",
                    "description": "Haiku 4.5 · Fastest for quick answers · $1/$5 per Mtok"
                }
            ]
        })
    }

    /// The live list drives the picker: alias ids the CLI accepts, its own
    /// labels/descriptions, and its own effort ladder (never a guessed one).
    #[test]
    fn parse_live_list_models_shape() {
        let models = parse_discovered_models(&live_list_models_payload());
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "opus[1m]", "sonnet", "haiku"]);
        assert_eq!(models[0].label, "Default (recommended)");
        assert_eq!(models[2].label, "Sonnet");
        assert!(
            models[2]
                .description
                .as_deref()
                .unwrap()
                .starts_with("Sonnet 5")
        );

        // `low high xhigh max` + the komet-only modes riding them.
        assert_eq!(
            models[1].reasoning_levels,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
                ReasoningLevel::Ultracode,
                ReasoningLevel::Ultrathink,
            ]
        );
        // Haiku advertises no effort levels: thinking toggle only.
        assert!(models[3].reasoning_levels.is_empty());
        assert_eq!(models[3].options.len(), 1);
        assert_eq!(models[3].options[0].id, "thinking");

        // `[1m]` rows are already long-context — no 200K/1M select to double
        // the suffix on the wire — while the plain `sonnet` alias gets one.
        assert!(models[1].options.iter().all(|o| o.id != "contextWindow"));
        assert!(models[2].options.iter().any(|o| o.id == "contextWindow"));
        // Fast mode comes from the wire, not from the id spelling.
        assert!(models[0].options.iter().any(|o| o.id == "fastMode"));
        assert!(models[2].options.iter().all(|o| o.id != "fastMode"));
    }

    /// `to_effort` must not clamp away the `xhigh` the CLI advertises for its
    /// alias ids, and must keep clamping it for older families.
    #[test]
    fn alias_ids_keep_their_advertised_xhigh() {
        for alias in ["default", "opus", "opus[1m]", "sonnet"] {
            assert!(supports_xhigh(alias), "{alias}");
            assert_eq!(
                to_effort(Some(ReasoningLevel::XHigh), Some(alias)),
                Some("xhigh"),
                "{alias}"
            );
        }
        assert!(!supports_xhigh("claude-opus-4-5"));
        assert!(!supports_xhigh("claude-sonnet-4-5"));
        assert_eq!(
            to_effort(Some(ReasoningLevel::XHigh), Some("claude-opus-4-5")),
            Some("max")
        );
        // Haiku stays effort-less (top-level thinking is its own toggle).
        assert_eq!(to_effort(Some(ReasoningLevel::High), Some("haiku")), None);
    }

    /// Older builds answer `supported_models` with `{id, name}` rows and no
    /// capability fields: the curated ladders still cover them.
    #[test]
    fn legacy_supported_models_shape_keeps_curated_ladders() {
        let payload = serde_json::json!({
            "models": [
                { "id": "claude-opus-5", "name": "Opus 5 (Live)" },
                { "id": "claude-sonnet-5", "name": "Sonnet 5 (Live)" }
            ]
        });
        let models = parse_discovered_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].reasoning_levels, FULL_LADDER);
        assert_eq!(models[1].reasoning_levels, XHIGH_LADDER);
    }

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
        // Haiku: leftover picker values must not become --effort.
        assert_eq!(
            to_effort(Some(ReasoningLevel::High), Some("claude-haiku-4-5")),
            None
        );
        assert_eq!(to_effort(Some(ReasoningLevel::Medium), Some("haiku")), None);
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
