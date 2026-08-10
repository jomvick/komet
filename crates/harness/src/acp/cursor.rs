//! Cursor ACP model enrichment.
//!
//! Cursor's ACP server has two picker shapes on the wire:
//! - **variants** (default): exploded ids like `auto-smart[optimize_for=balanced]`.
//! - **parameterized** (`clientCapabilities._meta.parameterizedModelPicker`):
//!   base ids (`auto-smart`) plus real config options (`optimize_for`,
//!   `effort`, `fast`, `thinking`, `context`).
//!
//! Komet opts into parameterized mode. Auto's Intelligence / Balance / Cost
//! tiers are the `optimize_for` select advertised on the session. HTML effort
//! badges still appear on some display names and are stripped.

use std::collections::{BTreeMap, BTreeSet};

use komet_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};
use serde_json::Value;

/// Strip Cursor's effort-badge HTML from a model name and return the plain
/// label plus the badge text when present.
///
/// The badge is a muted High/Medium/Low chip in Cursor's own UI — not markup
/// Komet can render — so callers put it on the description subline (or fold
/// it into a Reasoning ladder when the family has multiple variants).
pub(crate) fn clean_label(name: &str) -> (String, Option<String>) {
    let mut badges = Vec::new();
    let mut out = String::with_capacity(name.len());
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = name[i..].find('>') {
                let tag = &name[i..i + end + 1];
                let lower = tag.to_ascii_lowercase();
                if lower.starts_with("<span") {
                    let content_start = i + end + 1;
                    if let Some(close) = name[content_start..].to_ascii_lowercase().find("</span>")
                    {
                        let badge = name[content_start..content_start + close].trim();
                        if !badge.is_empty() {
                            badges.push(badge.to_owned());
                        }
                        i = content_start + close + "</span>".len();
                        continue;
                    }
                }
                // Drop any other tag wholesale.
                i += end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    let mut label = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let badge = badges.into_iter().next();
    // Display names sometimes duplicate the badge in plain text
    // ("Example (Low)" + span Low). Trim a trailing copy so the subline is
    // the only place it appears.
    if let Some(badge) = badge.as_deref() {
        for suffix in [
            format!(" ({badge})"),
            format!(" {badge}"),
            format!("({badge})"),
        ] {
            if let Some(stripped) = label
                .strip_suffix(&suffix)
                .or_else(|| label.strip_suffix(&suffix.to_ascii_lowercase()))
            {
                label = stripped.trim_end().to_owned();
                break;
            }
        }
    }
    (label, badge)
}

/// Bracket params on a Cursor model id (`key=value` pairs inside `[…]`).
pub(crate) fn parse_params(model_id: &str) -> BTreeMap<String, String> {
    let Some(start) = model_id.find('[') else {
        return BTreeMap::new();
    };
    let Some(end) = model_id.rfind(']') else {
        return BTreeMap::new();
    };
    if end <= start + 1 {
        return BTreeMap::new();
    }
    let mut params = BTreeMap::new();
    for part in model_id[start + 1..end].split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        if !k.is_empty() {
            params.insert(k.to_owned(), v.to_owned());
        }
    }
    params
}

fn base_id(model_id: &str) -> &str {
    model_id.split_once('[').map(|(b, _)| b).unwrap_or(model_id)
}

/// Family key used to collapse effort variants (`example-low` → `example`).
pub(crate) fn family_key(model_id: &str) -> String {
    let base = base_id(model_id);
    for suffix in ["-low", "-medium", "-high", "-xhigh", "-max"] {
        if let Some(stem) = base.strip_suffix(suffix) {
            return stem.to_owned();
        }
    }
    base.to_owned()
}

/// Effort / reasoning value declared on a Cursor model row, from brackets,
/// id suffix, or the HTML badge.
pub(crate) fn effort_of(model_id: &str, badge: Option<&str>) -> Option<String> {
    let params = parse_params(model_id);
    for key in ["effort", "reasoning", "reasoning_effort"] {
        if let Some(v) = params.get(key) {
            return Some(v.to_ascii_lowercase());
        }
    }
    let base = base_id(model_id);
    for (suffix, value) in [
        ("-low", "low"),
        ("-medium", "medium"),
        ("-high", "high"),
        ("-xhigh", "xhigh"),
        ("-max", "max"),
    ] {
        if base.ends_with(suffix) {
            return Some(value.into());
        }
    }
    badge.map(|b| b.trim().to_ascii_lowercase())
}

pub(crate) fn reasoning_from_effort(value: &str) -> Option<ReasoningLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" | "x-high" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        _ => None,
    }
}

fn effort_for_level(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        // Cursor never advertises these; map to the top of its ladder.
        ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
    }
}

/// Strip a Cursor exploded-variant suffix (`auto-smart[optimize_for=cost]` →
/// `auto-smart`) so a saved id from the old picker still matches parameterized
/// `session/set_config_option`.
pub(crate) fn strip_variant_suffix(id: &str) -> &str {
    id.split_once('[').map(|(b, _)| b).unwrap_or(id)
}

/// Auto's Optimize For trait (Intelligence / Balance / Cost). Injected on
/// `auto-smart` even when the current wire model isn't Auto — Cursor's ACP
/// session only lists parameters for the selected model.
pub(crate) fn optimize_for_option(current: Option<&str>) -> ModelOption {
    ModelOption {
        id: "optimize_for".into(),
        label: "Optimize For".into(),
        choices: vec![
            ModelOptionChoice {
                id: "intelligence".into(),
                label: "Intelligence".into(),
            },
            ModelOptionChoice {
                id: "balanced".into(),
                label: "Balance".into(),
            },
            ModelOptionChoice {
                id: "cost".into(),
                label: "Cost".into(),
            },
        ],
        default_choice: match current {
            Some(v @ ("intelligence" | "cost")) => v.to_owned(),
            _ => "balanced".into(),
        },
    }
}

fn mode_option(current: Option<&str>) -> ModelOption {
    let default = match current {
        Some("plan") => "plan",
        Some("ask") => "ask",
        _ => "agent",
    };
    ModelOption {
        id: "mode".into(),
        label: "Mode".into(),
        choices: vec![
            ModelOptionChoice {
                id: "agent".into(),
                label: "Agent".into(),
            },
            ModelOptionChoice {
                id: "plan".into(),
                label: "Plan".into(),
            },
            ModelOptionChoice {
                id: "ask".into(),
                label: "Ask".into(),
            },
        ],
        default_choice: default.into(),
    }
}

fn param_description(params: &BTreeMap<String, String>, badge: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ctx) = params.get("context") {
        parts.push(ctx.clone());
    }
    let effort = params
        .get("effort")
        .or_else(|| params.get("reasoning"))
        .or_else(|| params.get("reasoning_effort"))
        .cloned()
        .or_else(|| badge.map(str::to_owned));
    if let Some(effort) = effort {
        // Title-case the badge-style label Cursor uses in HTML.
        let pretty = match effort.to_ascii_lowercase().as_str() {
            "xhigh" | "x-high" => "XHigh".into(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => other.to_owned(),
                }
            }
        };
        parts.push(pretty);
    }
    if params.get("fast").is_some_and(|v| v == "true")
        || params.get("speed").is_some_and(|v| v == "true")
    {
        parts.push("Fast".into());
    }
    if params.get("thinking").is_some_and(|v| v == "true") {
        parts.push("Thinking".into());
    }
    if params.get("optimize_for").is_some_and(|v| v == "balanced") {
        parts.push("Balanced".into());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn config_options(session: &Value) -> &[Value] {
    session
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
}

fn option_current<'a>(options: &'a [Value], id: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|o| o.get("currentValue").and_then(Value::as_str))
}

fn model_option_from_config(option: &Value) -> Option<ModelOption> {
    let id = option.get("id").and_then(Value::as_str)?;
    let label = option.get("name").and_then(Value::as_str).unwrap_or(id);
    let choices: Vec<ModelOptionChoice> = option
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|c| {
            let cid = c.get("value").and_then(Value::as_str)?;
            Some(ModelOptionChoice {
                id: cid.to_owned(),
                label: c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(cid)
                    .to_owned(),
            })
        })
        .collect();
    if choices.len() < 2 {
        return None;
    }
    let default_choice = option
        .get("currentValue")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| choices.first().map(|c| c.id.clone()))?;
    Some(ModelOption {
        id: id.to_owned(),
        label: label.to_owned(),
        choices,
        default_choice,
    })
}

fn thought_ladder(option: &Value) -> Option<Vec<ReasoningLevel>> {
    let mut ladder: Vec<ReasoningLevel> = option
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|c| c.get("value").and_then(Value::as_str))
        .filter_map(reasoning_from_effort)
        .collect();
    ladder.sort();
    ladder.dedup();
    (ladder.len() >= 2).then_some(ladder)
}

fn is_auto_smart(id: &str) -> bool {
    strip_variant_suffix(id) == "auto-smart"
}

/// Majority base ids (no `[key=value]` suffix) → parameterized picker.
fn looks_parameterized(models: &[Model]) -> bool {
    if models.is_empty() {
        return true;
    }
    let exploded = models
        .iter()
        .filter(|m| parse_params(&m.id).len() > 0)
        .count();
    exploded * 2 < models.len()
}

/// Rewrite the wire model list into Komet's picker shape.
pub(crate) fn enrich_models(models: Vec<Model>, session: &Value) -> Vec<Model> {
    let options = config_options(session);
    let mode_current = option_current(options, "mode");
    if looks_parameterized(&models) {
        enrich_parameterized(models, options, mode_current)
    } else {
        enrich_exploded(models, mode_current)
    }
}

/// Parameterized catalog: base ids + real config options. Cursor's ACP
/// session only lists parameters for the *currently selected* model, so we
/// never copy those onto every row — Auto always gets Optimize For (from
/// the wire, or the public Intelligence/Balance/Cost defaults), and the
/// current model's extra options/ladder stay on that row alone.
fn enrich_parameterized(
    models: Vec<Model>,
    options: &[Value],
    mode_current: Option<&str>,
) -> Vec<Model> {
    let mode = mode_option(mode_current);
    let optimize = options
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some("optimize_for"))
        .and_then(model_option_from_config)
        .unwrap_or_else(|| optimize_for_option(option_current(options, "optimize_for")));
    let current_id = option_current(options, "model").map(strip_variant_suffix);
    let current_extras: Vec<ModelOption> = options
        .iter()
        .filter(|o| {
            matches!(
                o.get("category").and_then(Value::as_str),
                Some("model_config")
            ) && o.get("id").and_then(Value::as_str) != Some("optimize_for")
        })
        .filter_map(model_option_from_config)
        .collect();
    let mut current_ladder = Vec::new();
    let mut current_thought_toggles = Vec::new();
    for option in options
        .iter()
        .filter(|o| o.get("category").and_then(Value::as_str) == Some("thought_level"))
    {
        if let Some(ladder) = thought_ladder(option) {
            current_ladder = ladder;
        } else if let Some(toggle) = model_option_from_config(option) {
            current_thought_toggles.push(toggle);
        }
    }

    models
        .into_iter()
        .map(|mut model| {
            let (label, _) = clean_label(&model.label);
            model.label = label;
            model.id = strip_variant_suffix(&model.id).to_owned();
            // Discovery clones the current model's wire traits onto every
            // row; those are wrong under parameterized mode.
            model.options.clear();
            model.reasoning_levels.clear();
            model.options.push(mode.clone());
            if is_auto_smart(&model.id) {
                model.options.push(optimize.clone());
            }
            if current_id == Some(model.id.as_str()) {
                model.options.extend(current_extras.iter().cloned());
                model
                    .options
                    .extend(current_thought_toggles.iter().cloned());
                model.reasoning_levels = current_ladder.clone();
            }
            model
        })
        .collect()
}

/// Legacy exploded-variant catalog (no parameterizedModelPicker): cleaned
/// labels, Mode, and collapsed effort families whose Reasoning ladder
/// switches the model id.
fn enrich_exploded(models: Vec<Model>, mode_current: Option<&str>) -> Vec<Model> {
    // Preserve wire order while grouping by family.
    let mut family_order: Vec<String> = Vec::new();
    let mut families: BTreeMap<String, Vec<Model>> = BTreeMap::new();
    for model in models {
        let key = family_key(&model.id);
        if !families.contains_key(&key) {
            family_order.push(key.clone());
        }
        families.entry(key).or_default().push(model);
    }

    let mode = mode_option(mode_current);
    let mut out = Vec::with_capacity(family_order.len());
    for key in family_order {
        let members = families.remove(&key).unwrap_or_default();
        if members.is_empty() {
            continue;
        }
        // Map each member → (effort token, cleaned model).
        let annotated: Vec<(Option<String>, Model)> = members
            .into_iter()
            .map(|mut model| {
                let (label, badge) = clean_label(&model.label);
                let params = parse_params(&model.id);
                let effort = effort_of(&model.id, badge.as_deref());
                model.label = label;
                if model.description.as_ref().is_none_or(|d| d.is_empty()) {
                    model.description = param_description(&params, badge.as_deref());
                }
                (effort, model)
            })
            .collect();

        let distinct_efforts: BTreeSet<String> =
            annotated.iter().filter_map(|(e, _)| e.clone()).collect();
        if annotated.len() > 1 && distinct_efforts.len() > 1 {
            // Collapse: keep the highest-effort row's id as the default pick,
            // expose every effort as a Reasoning choice.
            let mut ladder: Vec<ReasoningLevel> = distinct_efforts
                .iter()
                .filter_map(|e| reasoning_from_effort(e))
                .collect();
            ladder.sort();
            ladder.dedup();
            let default = annotated
                .iter()
                .rev()
                .find(|(e, _)| {
                    e.as_deref()
                        .is_some_and(|v| matches!(v, "high" | "xhigh" | "max"))
                })
                .or_else(|| annotated.last())
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| annotated[0].1.clone());
            let (label, _) = clean_label(&default.label);
            // Prefer the family stem name without "(Low)" / effort noise.
            let label = annotated
                .iter()
                .map(|(_, m)| m.label.clone())
                .min_by_key(|l| l.len())
                .unwrap_or(label);
            out.push(Model {
                id: default.id,
                label,
                description: Some("Effort variants".into()),
                reasoning_levels: ladder,
                options: vec![mode.clone()],
            });
            continue;
        }

        for (_, mut model) in annotated {
            // Locked single-variant params stay on the description; Mode is
            // the one trait every Cursor row can actually change.
            model.options.insert(0, mode.clone());
            // A lone effort value is not a ladder — don't pretend it is.
            model.reasoning_levels.clear();
            out.push(model);
        }
    }
    out
}

/// Pick the advertised Cursor model id for a requested id + preferred efforts.
///
/// Returns `None` when the family has no effort variants to choose among, so
/// the generic ACP model picker (1M compose, exact match) stays in charge.
/// When siblings differ by effort, the first preferred effort that matches
/// an advertised row wins.
pub(crate) fn pick_model_id(
    requested: &str,
    efforts: &[&str],
    available: &[&str],
) -> Option<String> {
    let family = family_key(requested);
    let siblings: Vec<&str> = available
        .iter()
        .copied()
        .filter(|id| family_key(id) == family)
        .collect();
    if siblings.is_empty() {
        return None;
    }
    let distinct: BTreeSet<String> = siblings
        .iter()
        .filter_map(|id| effort_of(id, None))
        .collect();
    if distinct.len() <= 1 {
        return None;
    }
    for effort in efforts {
        let want = effort.to_ascii_lowercase();
        if let Some(id) = siblings
            .iter()
            .find(|id| effort_of(id, None).as_deref() == Some(want.as_str()))
        {
            return Some((*id).to_owned());
        }
    }
    if available.contains(&requested) {
        return Some(requested.to_owned());
    }
    siblings
        .iter()
        .find(|id| {
            effort_of(id, None)
                .as_deref()
                .is_some_and(|v| matches!(v, "high" | "xhigh" | "max"))
        })
        .or_else(|| siblings.first())
        .map(|id| (*id).to_owned())
}

/// Preference-ordered effort tokens for a Komet reasoning pick, used when
/// resolving a Cursor family variant.
pub(crate) fn effort_tokens(level: ReasoningLevel) -> Vec<&'static str> {
    let primary = effort_for_level(level);
    match level {
        ReasoningLevel::XHigh => vec![primary, "max", "high"],
        ReasoningLevel::Max => vec![primary, "xhigh", "high"],
        ReasoningLevel::High => vec![primary, "xhigh", "medium"],
        _ => vec![primary],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::Model;
    use serde_json::json;

    #[test]
    fn clean_label_strips_span_badges_without_duplicating() {
        let (label, badge) = clean_label("Example <span>High</span>");
        assert_eq!(label, "Example");
        assert_eq!(badge.as_deref(), Some("High"));

        let (label, badge) = clean_label("Example (Low) <span>Low</span>");
        assert_eq!(label, "Example");
        assert_eq!(badge.as_deref(), Some("Low"));

        let (label, badge) = clean_label("Opus 5");
        assert_eq!(label, "Opus 5");
        assert!(badge.is_none());
    }

    #[test]
    fn parse_params_reads_bracket_suffix() {
        let p = parse_params("claude-opus-5[thinking=true,context=1m,effort=high,fast=false]");
        assert_eq!(p.get("effort").map(String::as_str), Some("high"));
        assert_eq!(p.get("context").map(String::as_str), Some("1m"));
        assert_eq!(p.get("fast").map(String::as_str), Some("false"));
        assert!(parse_params("gemini-3.1-pro[]").is_empty());
        assert!(parse_params("gemini-3.1-pro").is_empty());
    }

    #[test]
    fn enrich_collapses_effort_family_and_injects_mode() {
        let models = enrich_models(
            vec![
                Model {
                    id: "example[reasoning_effort=high]".into(),
                    label: "Example <span>High</span>".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                },
                Model {
                    id: "example-low[]".into(),
                    label: "Example (Low) <span>Low</span>".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                },
                Model {
                    id: "example-medium[]".into(),
                    label: "Example (Medium) <span>Medium</span>".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                },
                Model {
                    id: "composer-2.5[fast=true]".into(),
                    label: "Composer 2.5".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                },
            ],
            &json!({ "configOptions": [{ "id": "mode", "currentValue": "agent" }] }),
        );
        assert_eq!(models.len(), 2);
        let example = models.iter().find(|m| m.label == "Example").unwrap();
        assert_eq!(
            example.reasoning_levels,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
        assert!(example.options.iter().any(|o| o.id == "mode"));
        let composer = models.iter().find(|m| m.label == "Composer 2.5").unwrap();
        assert!(composer.reasoning_levels.is_empty());
        assert_eq!(composer.description.as_deref(), Some("Fast"));
        assert_eq!(composer.options[0].id, "mode");
    }

    #[test]
    fn parameterized_catalog_injects_optimize_for_on_auto_only() {
        let models = enrich_models(
            vec![
                Model {
                    id: "auto-smart".into(),
                    label: "Auto".into(),
                    description: None,
                    reasoning_levels: vec![ReasoningLevel::High],
                    options: vec![],
                },
                Model {
                    id: "composer-2.5".into(),
                    label: "Composer 2.5".into(),
                    description: None,
                    reasoning_levels: vec![ReasoningLevel::High],
                    options: vec![],
                },
                Model {
                    id: "claude-opus-5".into(),
                    label: "Opus 5".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                },
            ],
            &json!({
                "configOptions": [
                    { "id": "mode", "category": "mode", "currentValue": "agent" },
                    {
                        "id": "model",
                        "category": "model",
                        "currentValue": "composer-2.5",
                    },
                    {
                        "id": "fast",
                        "category": "model_config",
                        "name": "Fast",
                        "type": "select",
                        "currentValue": "true",
                        "options": [
                            { "value": "false", "name": "Off" },
                            { "value": "true", "name": "Fast" },
                        ],
                    },
                ]
            }),
        );
        let auto = models.iter().find(|m| m.id == "auto-smart").unwrap();
        let optimize = auto
            .options
            .iter()
            .find(|o| o.id == "optimize_for")
            .expect("Auto must expose Optimize For");
        assert_eq!(
            optimize
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["intelligence", "balanced", "cost"]
        );
        assert!(auto.reasoning_levels.is_empty());
        let composer = models.iter().find(|m| m.id == "composer-2.5").unwrap();
        assert!(composer.options.iter().any(|o| o.id == "fast"));
        assert!(composer.options.iter().all(|o| o.id != "optimize_for"));
        let opus = models.iter().find(|m| m.id == "claude-opus-5").unwrap();
        assert!(opus.options.iter().all(|o| o.id == "mode"));
        assert!(opus.reasoning_levels.is_empty());
    }

    #[test]
    fn pick_model_id_switches_family_by_effort() {
        let available = [
            "example[reasoning_effort=high]",
            "example-low[]",
            "example-medium[]",
            "composer-2.5[fast=true]",
        ];
        assert_eq!(
            pick_model_id("example[reasoning_effort=high]", &["low"], &available).as_deref(),
            Some("example-low[]")
        );
        assert_eq!(
            pick_model_id("example-low[]", &["high"], &available).as_deref(),
            Some("example[reasoning_effort=high]")
        );
        assert_eq!(
            pick_model_id("composer-2.5[fast=true]", &["high"], &available).as_deref(),
            None,
            "single-variant families defer to the generic picker"
        );
    }
}
