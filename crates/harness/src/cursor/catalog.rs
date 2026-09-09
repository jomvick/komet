//! Cursor Agent model catalog.
//!
//! The picker shows Cursor-native families only — Auto, Composer, Grok —
//! taken live from `cursor-agent --list-models` (account-specific exploded
//! ids, folded into parameterized rows). New Composer / Grok versions appear
//! automatically when the CLI lists them. The SDK catalog is a fallback.
//! Runs still go through `@cursor/sdk`, so [`sdk_model_id`] maps a Cursor
//! Agent family id back to the SDK's `ModelSelection.id`.

use std::collections::BTreeMap;

use komet_proto::{Model, ModelOption, ModelOptionChoice, SlashCommand};
use serde_json::Value;

/// Longer effort tokens first so `extra-high` wins over `high`.
const EFFORT_TOKENS: &[&str] = &[
    "extra-high",
    "xhigh",
    "minimal",
    "medium",
    "high",
    "none",
    "low",
    "max",
];

/// Map a Cursor Agent family id (or a leftover SDK alias) to the id
/// `@cursor/sdk` `Agent.create` accepts.
pub(crate) fn sdk_model_id(id: &str) -> &str {
    match id {
        "default" => "auto",
        other => other
            .strip_prefix("cursor-")
            .filter(|rest| rest.starts_with("grok"))
            .unwrap_or(other),
    }
}

/// Inverse of [`sdk_model_id`]: present the Cursor Agent family id in the picker.
fn cli_model_id(sdk_id: &str) -> String {
    match sdk_id {
        "default" | "auto-smart" => "auto".into(),
        other if other.starts_with("grok") => format!("cursor-{other}"),
        other => other.to_owned(),
    }
}

/// Cursor-owned families: Auto, Composer (any version), Grok (any version).
/// Version numbers are not hardcoded — a new `composer-3` or `cursor-grok-4.7`
/// from `cursor-agent --list-models` is kept automatically.
pub(crate) fn is_native_cursor_model(id: &str) -> bool {
    let id = id.strip_prefix("cursor-").unwrap_or(id);
    matches!(id, "auto" | "default" | "auto-smart" | "composer" | "grok")
        || id.starts_with("composer-")
        || id.starts_with("grok-")
}

/// Built-in slash commands native to the cursor-agent TUI. Skip TUI-only
/// dialogs (vim, quit, help, copy, logout, about, logs, update).
pub(crate) fn static_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "summarize".into(),
            description: "Summarize the conversation to reduce context".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "compress".into(),
            description: "Summarize the conversation (alias of /summarize)".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "plan".into(),
            description: "Switch to Plan mode or submit a prompt in Plan mode".into(),
            input_hint: Some("[prompt]".into()),
        },
        SlashCommand {
            name: "ask".into(),
            description: "Toggle Ask mode for read-only questions".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "debug".into(),
            description: "Toggle Debug mode or submit a prompt in Debug mode".into(),
            input_hint: Some("[prompt]".into()),
        },
        SlashCommand {
            name: "goal".into(),
            description: "Give the agent a long-lived objective until complete".into(),
            input_hint: Some("[objective]".into()),
        },
        SlashCommand {
            name: "clear".into(),
            description: "Start a new chat session".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "rename".into(),
            description: "Rename the current chat session".into(),
            input_hint: Some("<name>".into()),
        },
        SlashCommand {
            name: "rewind".into(),
            description: "Jump back to a previous message".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "fork".into(),
            description: "Fork the current chat into a new session".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "model".into(),
            description: "Select a model".into(),
            input_hint: Some("[filter]".into()),
        },
        SlashCommand {
            name: "sandbox".into(),
            description: "Configure sandbox mode and network access".into(),
            input_hint: None,
        },
        SlashCommand {
            name: "mcp".into(),
            description: "Manage MCP servers and list tools".into(),
            input_hint: Some("[list|list-tools] [identifier]".into()),
        },
        SlashCommand {
            name: "shell".into(),
            description: "Enter Shell Mode".into(),
            input_hint: Some("[command]".into()),
        },
    ]
}

/// Fold `cursor-agent --list-models` text (`id - Label` lines) into picker rows.
pub(crate) fn fold_cli_models(text: &str) -> Vec<Model> {
    let mut families: Vec<Family> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (id, label) in parse_cli_lines(text) {
        let parsed = parse_exploded(&id);
        let slot = *index.entry(parsed.family.clone()).or_insert_with(|| {
            let idx = families.len();
            families.push(Family::new(&parsed, label));
            idx
        });
        families[slot].observe(&parsed);
    }
    families
        .into_iter()
        .map(Family::into_model)
        .filter(|m| is_native_cursor_model(&m.id))
        .collect()
}

/// Overlay SDK parameter schemas onto Cursor Agent families (same order / ids
/// / labels). Families the SDK does not know keep the inferred options.
#[cfg(test)]
pub(crate) fn merge_cli_with_sdk(cli: Vec<Model>, sdk: &[Model]) -> Vec<Model> {
    cli.into_iter()
        .map(|mut model| {
            if let Some(sdk_row) = sdk.iter().find(|s| ids_match(&model.id, &s.id)) {
                if !sdk_row.options.is_empty() {
                    model.options = sdk_row.options.clone();
                }
            }
            model
        })
        .collect()
}

#[cfg(test)]
fn ids_match(cli_id: &str, sdk_id: &str) -> bool {
    cli_id == sdk_id || sdk_model_id(cli_id) == sdk_id || cli_id == cli_model_id(sdk_id)
}

/// `Cursor.models.list()` items → picker models. Auto's wire id is `default`;
/// present it as Cursor Agent's `auto`. Other SDK ids that differ from the
/// CLI family (`grok-4.6`) are rewritten to the Cursor Agent id.
pub(crate) fn map_model_items(items: &Value) -> Vec<Model> {
    let str_of = |v: &Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let items = items.as_array().map(|a| a.as_slice()).unwrap_or_default();
    let has_named_auto = items.iter().any(|item| {
        matches!(
            item.get("id").and_then(Value::as_str),
            Some("auto" | "auto-smart")
        )
    });
    items
        .iter()
        .filter_map(|item| {
            let raw_id = str_of(item, "id")?;
            // `default` is the alias twin of Auto (`auto` / `auto-smart`).
            if raw_id == "default" && has_named_auto {
                return None;
            }
            let id = cli_model_id(&raw_id);
            if !is_native_cursor_model(&id) {
                return None;
            }
            let label = str_of(item, "displayName").unwrap_or_else(|| id.clone());
            let default_variant = item
                .get("variants")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .find(|v| v.get("isDefault").and_then(Value::as_bool) == Some(true))
                .and_then(|v| v.get("params").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            let options: Vec<ModelOption> = item
                .get("parameters")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|p| {
                    let pid = str_of(p, "id")?;
                    let choices: Vec<ModelOptionChoice> = p
                        .get("values")
                        .and_then(Value::as_array)
                        .map(|a| a.as_slice())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|c| {
                            let cid = str_of(c, "value")?;
                            Some(ModelOptionChoice {
                                label: str_of(c, "displayName").unwrap_or_else(|| cid.clone()),
                                id: cid,
                            })
                        })
                        .collect();
                    if choices.is_empty() {
                        return None;
                    }
                    let default_choice = default_variant
                        .iter()
                        .find(|dv| dv.get("id").and_then(Value::as_str) == Some(pid.as_str()))
                        .and_then(|dv| str_of(dv, "value"))
                        .unwrap_or_else(|| choices[0].id.clone());
                    Some(ModelOption {
                        label: str_of(p, "displayName").unwrap_or_else(|| pid.clone()),
                        id: pid,
                        choices,
                        default_choice,
                    })
                })
                .collect();
            Some(Model {
                id,
                label,
                description: str_of(item, "description"),
                reasoning_levels: Vec::new(),
                options,
            })
        })
        .collect()
}

/// Fallback when both `cursor-agent --list-models` and the SDK probe fail:
/// native Cursor Agent families only (Auto / Composer / Grok).
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "auto",
            "Auto",
            "Cursor picks the model per request",
            Vec::new(),
        ),
        model(
            "composer-2.5",
            "Composer 2.5",
            "Cursor's own fast coding model",
            vec![bool_option("fast", "Fast", false)],
        ),
        model(
            "cursor-grok-4.6",
            "Cursor Grok 4.6",
            "Cursor's Grok coding model",
            vec![
                effort_option("effort", &["low", "medium", "high", "xhigh"], "high"),
                bool_option("fast", "Fast", true),
            ],
        ),
    ]
}

fn model(id: &str, label: &str, description: &str, options: Vec<ModelOption>) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        reasoning_levels: Vec::new(),
        options,
    }
}

fn parse_cli_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (id, label) = line.split_once(" - ")?;
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return None;
            }
            Some((id.to_owned(), label.to_owned()))
        })
        .collect()
}

struct Parsed {
    family: String,
    effort: Option<String>,
    thinking: bool,
    fast: bool,
}

fn parse_exploded(id: &str) -> Parsed {
    let mut fast = false;
    let mut stem = id;
    if let Some(stripped) = stem.strip_suffix("-fast") {
        fast = true;
        stem = stripped;
    }
    for effort in EFFORT_TOKENS {
        let thinking_then = format!("-thinking-{effort}");
        if let Some(stripped) = stem.strip_suffix(&thinking_then) {
            return Parsed {
                family: stripped.to_owned(),
                effort: Some((*effort).to_owned()),
                thinking: true,
                fast,
            };
        }
        let effort_then = format!("-{effort}-thinking");
        if let Some(stripped) = stem.strip_suffix(&effort_then) {
            return Parsed {
                family: stripped.to_owned(),
                effort: Some((*effort).to_owned()),
                thinking: true,
                fast,
            };
        }
    }
    let mut thinking = false;
    if let Some(stripped) = stem.strip_suffix("-thinking") {
        thinking = true;
        stem = stripped;
    }
    let mut effort = None;
    for token in EFFORT_TOKENS {
        if let Some(stripped) = stem.strip_suffix(&format!("-{token}")) {
            effort = Some((*token).to_owned());
            stem = stripped;
            break;
        }
    }
    Parsed {
        family: stem.to_owned(),
        effort,
        thinking,
        fast,
    }
}

struct Family {
    id: String,
    label: String,
    efforts: Vec<String>,
    thinking: bool,
    fast: bool,
    default_effort: Option<String>,
    default_thinking: bool,
    default_fast: bool,
    saw_bare: bool,
}

impl Family {
    fn new(parsed: &Parsed, label: String) -> Self {
        Self {
            id: parsed.family.clone(),
            label: clean_label(&label),
            efforts: Vec::new(),
            thinking: false,
            fast: false,
            default_effort: parsed.effort.clone(),
            default_thinking: parsed.thinking,
            default_fast: parsed.fast,
            saw_bare: false,
        }
    }

    fn observe(&mut self, parsed: &Parsed) {
        if parsed.effort.is_none() && !parsed.thinking {
            self.saw_bare = true;
        }
        if let Some(effort) = &parsed.effort
            && !self.efforts.iter().any(|e| e == effort)
        {
            self.efforts.push(effort.clone());
        }
        self.thinking |= parsed.thinking;
        self.fast |= parsed.fast;
    }

    fn into_model(mut self) -> Model {
        if self.saw_bare && !self.efforts.is_empty() && !self.efforts.iter().any(|e| e == "medium")
        {
            self.efforts.push("medium".into());
        }
        let mut options = Vec::new();
        if !self.efforts.is_empty() {
            let default = self
                .default_effort
                .clone()
                .filter(|e| self.efforts.iter().any(|o| o == e))
                .or_else(|| self.efforts.iter().find(|e| *e == "medium").cloned())
                .unwrap_or_else(|| self.efforts[0].clone());
            options.push(effort_option(
                effort_param_id(&self.id),
                &self.efforts.iter().map(String::as_str).collect::<Vec<_>>(),
                &default,
            ));
        }
        if self.thinking {
            options.push(bool_option("thinking", "Thinking", self.default_thinking));
        }
        if self.fast {
            options.push(bool_option("fast", "Fast", self.default_fast));
        }
        Model {
            id: self.id,
            label: self.label,
            description: None,
            reasoning_levels: Vec::new(),
            options,
        }
    }
}

fn effort_param_id(family: &str) -> &'static str {
    if family.starts_with("gpt-") || family.starts_with("kimi-") || family.starts_with("glm-") {
        "reasoning"
    } else if family.contains("gemini-3.8") {
        "reasoning_effort"
    } else {
        "effort"
    }
}

fn effort_option(id: &str, values: &[&str], default: &str) -> ModelOption {
    let choices = values
        .iter()
        .map(|v| ModelOptionChoice {
            id: (*v).to_owned(),
            label: effort_label(v),
        })
        .collect::<Vec<_>>();
    let default_choice = if choices.iter().any(|c| c.id == default) {
        default.to_owned()
    } else {
        choices[0].id.clone()
    };
    ModelOption {
        id: id.into(),
        label: if id == "reasoning" || id == "reasoning_effort" {
            "Reasoning".into()
        } else {
            "Effort".into()
        },
        choices,
        default_choice,
    }
}

fn effort_label(value: &str) -> String {
    match value {
        "extra-high" | "xhigh" => "Extra High".into(),
        "none" => "None".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => other.to_owned(),
            }
        }
    }
}

fn bool_option(id: &str, label: &str, default_on: bool) -> ModelOption {
    ModelOption {
        id: id.into(),
        label: label.into(),
        choices: vec![
            ModelOptionChoice {
                id: "false".into(),
                label: "Off".into(),
            },
            ModelOptionChoice {
                id: "true".into(),
                label: "On".into(),
            },
        ],
        default_choice: if default_on { "true" } else { "false" }.into(),
    }
}

fn clean_label(label: &str) -> String {
    let mut s = label.replace("(default)", "").replace("(NO ZDR)", "");
    for word in [
        "Extra High",
        "1M",
        "Thinking",
        "Fast",
        "Minimal",
        "Medium",
        "High",
        "Low",
        "None",
        "Max",
    ] {
        s = s.replace(word, " ");
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CLI_SAMPLE: &str = "\
Available models

auto - Auto (default)
gpt-5.3-codex-low - Codex 5.3 Low
gpt-5.3-codex - Codex 5.3
cursor-grok-4.6-high-fast - Cursor Grok 4.6 Fast
composer-2.5 - Composer 2.5
composer-2.5-fast - Composer 2.5 Fast
claude-opus-5-thinking-high - Claude Opus 5 1M Thinking
claude-opus-5-thinking-high-fast - Claude Opus 5 1M Thinking Fast
claude-opus-5-low - Claude Opus 5 1M Low
claude-fable-5-1-thinking-high - Claude Fable 5.1 1M Thinking (NO ZDR)
claude-fable-5-thinking-high - Claude Fable 5 1M Thinking (NO ZDR)
claude-4.6-sonnet-medium - Claude Sonnet 4.6 1M
claude-4.6-sonnet-medium-thinking - Claude Sonnet 4.6 1M Thinking
kimi-k2.7-code - Kimi K2.7 Code
gpt-5.6-sol-medium - GPT-5.6 Sol 1M

Tip: use --model <id>
";

    #[test]
    fn fold_cli_models_keeps_native_auto_grok_composer() {
        let models = fold_cli_models(CLI_SAMPLE);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["auto", "cursor-grok-4.6", "composer-2.5"]);
        assert_eq!(models[0].label, "Auto");
        assert!(models[0].options.is_empty());
        assert_eq!(models[1].label, "Cursor Grok 4.6");
        assert_eq!(models[2].label, "Composer 2.5");

        let grok = &models[1];
        assert!(grok.options.iter().any(|o| o.id == "effort"));
        assert_eq!(
            grok.options
                .iter()
                .find(|o| o.id == "fast")
                .map(|o| o.default_choice.as_str()),
            Some("true")
        );
    }

    #[test]
    fn fold_picks_up_new_native_versions_from_cursor_agent() {
        let models = fold_cli_models(
            "auto - Auto\n\
             composer-3 - Composer 3\n\
             composer-3-fast - Composer 3 Fast\n\
             cursor-grok-4.7-high - Cursor Grok 4.7\n\
             claude-opus-6-high - Claude Opus 6\n",
        );
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["auto", "composer-3", "cursor-grok-4.7"]);
        assert!(!ids.iter().any(|id| id.contains("claude")));
    }

    #[test]
    fn map_model_items_keeps_auto_and_rewrites_sdk_ids() {
        let items = json!([
            {"id":"default","displayName":"Auto","aliases":["auto"]},
            {
                "id":"grok-4.6",
                "displayName":"Cursor Grok 4.6",
                "parameters":[{
                    "id":"effort",
                    "displayName":"Effort",
                    "values":[{"value":"low"},{"value":"high"}]
                }]
            }
        ]);
        let models = map_model_items(&items);
        assert_eq!(models[0].id, "auto");
        assert_eq!(models[0].label, "Auto");
        assert_eq!(models[1].id, "cursor-grok-4.6");
        assert_eq!(models[1].options[0].id, "effort");

        let with_foreign = map_model_items(&json!([
            {"id":"default","displayName":"Auto"},
            {"id":"claude-opus-5","displayName":"Claude Opus 5"},
            {"id":"composer-2.5","displayName":"Composer 2.5"}
        ]));
        assert_eq!(
            with_foreign
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "composer-2.5"]
        );

        let with_twin = map_model_items(&json!([
            {
                "id":"auto-smart",
                "displayName":"Auto",
                "parameters":[{"id":"optimize_for","values":[{"value":"balanced"}]}]
            },
            {"id":"default","displayName":"Auto","aliases":["auto"]}
        ]));
        assert_eq!(with_twin.len(), 1);
        assert_eq!(with_twin[0].id, "auto");
        assert_eq!(with_twin[0].options[0].id, "optimize_for");
    }

    #[test]
    fn merge_keeps_cli_order_and_sdk_options() {
        let cli = fold_cli_models("auto - Auto\ncursor-grok-4.6-high - Cursor Grok 4.6\n");
        let sdk = map_model_items(&json!([
            {"id":"default","displayName":"Auto"},
            {
                "id":"grok-4.6",
                "displayName":"Grok",
                "parameters":[{
                    "id":"effort",
                    "values":[{"value":"low"},{"value":"medium"},{"value":"high"},{"value":"xhigh"}]
                }]
            }
        ]));
        let merged = merge_cli_with_sdk(cli, &sdk);
        assert_eq!(merged[0].id, "auto");
        assert_eq!(merged[1].id, "cursor-grok-4.6");
        assert_eq!(merged[1].label, "Cursor Grok 4.6");
        assert_eq!(
            merged[1].options[0]
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn sdk_model_id_round_trips_cursor_agent_families() {
        assert_eq!(sdk_model_id("auto"), "auto");
        assert_eq!(sdk_model_id("cursor-grok-4.6"), "grok-4.6");
        assert_eq!(sdk_model_id("cursor-grok-4.7"), "grok-4.7");
        assert_eq!(sdk_model_id("composer-2.5"), "composer-2.5");
        assert_eq!(cli_model_id("grok-4.6"), "cursor-grok-4.6");
        assert_eq!(cli_model_id("grok-4.7"), "cursor-grok-4.7");
        assert_eq!(cli_model_id("default"), "auto");
        assert!(is_native_cursor_model("composer-3"));
        assert!(is_native_cursor_model("cursor-grok-4.7"));
        assert!(!is_native_cursor_model("claude-opus-5"));
    }

    #[test]
    fn static_catalog_is_current_cursor_agent_families() {
        let models = static_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["auto", "composer-2.5", "cursor-grok-4.6"]);
    }

    #[test]
    fn static_commands_cover_cursor_agent_tui_builtins() {
        let commands = static_commands();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"summarize"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"goal"));
        assert!(names.contains(&"clear"));
        assert!(!names.contains(&"quit"));
        assert!(!names.contains(&"vim"));
    }
}
