//! Model catalog + effort mapping for Google Antigravity CLI (`agy`).
//!
//! Exposes the real models supported by the Antigravity CLI / Gemini runtime,
//! with reasoning ladders and context options.

use komet_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

pub fn default_model() -> &'static str {
    "gemini-3.8-flash-high"
}

/// The 1M/2M context-window selector available on Gemini frontier models.
pub(crate) fn context_window() -> ModelOption {
    ModelOption {
        id: "contextWindow".into(),
        label: "Context Window".into(),
        choices: vec![
            ModelOptionChoice {
                id: "1m".into(),
                label: "1M".into(),
            },
            ModelOptionChoice {
                id: "2m".into(),
                label: "2M".into(),
            },
        ],
        default_choice: "1m".into(),
    }
}

pub(crate) const STANDARD_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
];

pub(crate) const MINIMAL_LADDER: &[ReasoningLevel] = &[ReasoningLevel::Low];

const HIGH_ONLY: &[ReasoningLevel] = &[ReasoningLevel::High];
const MEDIUM_ONLY: &[ReasoningLevel] = &[ReasoningLevel::Medium];
const LOW_ONLY: &[ReasoningLevel] = &[ReasoningLevel::Low];

fn model(id: &str, label: &str, description: &str, options: Vec<ModelOption>) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder_for(id).to_vec(),
        options,
    }
}

/// Effort already encoded in the model id (`gemini-3.8-flash-high`,
/// `gpt-oss-120b-medium`, …). `agy` rejects a *different* `--effort` as a
/// conflict (`--model gemini-3.8-flash-high conflicts with --effort=medium`).
fn baked_effort(model: &str) -> Option<&'static str> {
    if model.ends_with("-high") {
        Some("high")
    } else if model.ends_with("-medium") {
        Some("medium")
    } else if model.ends_with("-low") {
        Some("low")
    } else {
        None
    }
}

/// Reasoning ladder advertised for a model. Claude rejects `--effort`
/// outright, so it has no picker; suffixed Gemini/GPT ids expose the single
/// baked tier (switching effort means picking a different model).
pub(crate) fn ladder_for(id: &str) -> &'static [ReasoningLevel] {
    if id.contains("claude") {
        return &[];
    }
    match baked_effort(id) {
        Some("high") => HIGH_ONLY,
        Some("medium") => MEDIUM_ONLY,
        Some("low") => LOW_ONLY,
        _ if id.contains("lite") => MINIMAL_LADDER,
        _ => STANDARD_LADDER,
    }
}

/// Map the reasoning level to the `--effort` flag value accepted by `agy`.
///
/// `agy` only accepts `low`/`medium`/`high`, and only for models that neither
/// bake effort into the id nor reject the flag (Claude: `--effort is not
/// supported for model "claude-sonnet-4-6"`).
pub fn to_effort(level: Option<ReasoningLevel>, model: Option<&str>) -> Option<&'static str> {
    let model = match model {
        Some(m) if !m.is_empty() => m,
        _ => default_model(),
    };
    if model.contains("claude") || baked_effort(model).is_some() {
        return None;
    }
    match level? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High
        | ReasoningLevel::XHigh
        | ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => Some("high"),
    }
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse the text output of `agy models` into a list of models.
pub fn parse_models(output: &str) -> Vec<Model> {
    let mut models = Vec::new();
    let static_list = static_models();
    // agy models prints an animated terminal spinner separated by '\r'.
    // Normalizing '\r' into '\n' separates spinner frames from actual model lines.
    let normalized = output.replace('\r', "\n");
    let cleaned = strip_ansi(&normalized);

    for line in cleaned.lines() {
        let trimmed = line.trim();
        // Skip progress indicators (like "Fetching available models..."), headers, or empty lines
        if trimmed.is_empty()
            || trimmed.contains("Fetching available models")
            || trimmed.starts_with("Usage")
            || trimmed.starts_with("Flags:")
            || trimmed.starts_with("List available")
            || trimmed.starts_with('-')
        {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        let id = parts[0];
        // Ensure id looks like a valid model identifier
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            continue;
        }
        let label = if parts.len() > 1 {
            parts[1..].join(" ")
        } else {
            static_list
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.label.clone())
                .unwrap_or_else(|| id.to_string())
        };
        let options = if id.contains("gemini") {
            vec![context_window()]
        } else {
            Vec::new()
        };
        let description = static_list
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.description.as_deref())
            .unwrap_or("");
        models.push(model(id, &label, description, options));
    }
    if models.is_empty() {
        return static_list;
    }
    models
}

/// Curated official Antigravity CLI models (matching live `agy models` output —
/// keep in sync; stale ids are rejected by agy with "Requested model is not
/// valid").
pub fn static_models() -> Vec<Model> {
    vec![
        model(
            "gemini-3.8-flash-high",
            "Gemini 3.8 Flash (High)",
            "Fast frontier model with high reasoning & coding",
            vec![context_window()],
        ),
        model(
            "gemini-3.8-flash-medium",
            "Gemini 3.8 Flash (Medium)",
            "Fast frontier model with medium reasoning & coding",
            vec![context_window()],
        ),
        model(
            "gemini-3.8-flash-low",
            "Gemini 3.8 Flash (Low)",
            "Fast frontier model with low reasoning & coding",
            vec![context_window()],
        ),
        model(
            "gemini-3.7-flash-high",
            "Gemini 3.7 Flash (High)",
            "Frontier model with high reasoning effort",
            vec![context_window()],
        ),
        model(
            "gemini-3.7-flash-medium",
            "Gemini 3.7 Flash (Medium)",
            "Frontier model with medium reasoning effort",
            vec![context_window()],
        ),
        model(
            "gemini-3.7-flash-low",
            "Gemini 3.7 Flash (Low)",
            "Frontier model with low reasoning effort",
            vec![context_window()],
        ),
        model(
            "gemini-3.6-flash-high",
            "Gemini 3.6 Flash (High)",
            "Lightweight fast model with high reasoning",
            vec![context_window()],
        ),
        model(
            "gemini-3.6-flash-medium",
            "Gemini 3.6 Flash (Medium)",
            "Lightweight fast model with medium reasoning",
            vec![context_window()],
        ),
        model(
            "gemini-3.6-flash-low",
            "Gemini 3.6 Flash (Low)",
            "Lightweight fast model with low reasoning",
            vec![context_window()],
        ),
        model(
            "gemini-3.1-pro-high",
            "Gemini 3.1 Pro (High)",
            "Advanced reasoning and deep code analysis",
            vec![context_window()],
        ),
        model(
            "gemini-3.1-pro-low",
            "Gemini 3.1 Pro (Low)",
            "Advanced reasoning and deep code analysis (fast)",
            vec![context_window()],
        ),
        model(
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6 (Thinking)",
            "Anthropic hybrid reasoning model via Antigravity",
            Vec::new(),
        ),
        model(
            "claude-opus-4-6-thinking",
            "Claude Opus 4.6 (Thinking)",
            "Anthropic deep reasoning model via Antigravity",
            Vec::new(),
        ),
        model(
            "gpt-oss-120b-medium",
            "GPT-OSS 120B (Medium)",
            "Open-weights frontier model via Antigravity",
            Vec::new(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_mapping_works() {
        // Unsuffixed models (none in the current catalog; default is baked).
        assert_eq!(to_effort(None, Some("gemini-future")), None);
        assert_eq!(
            to_effort(Some(ReasoningLevel::Low), Some("gemini-future")),
            Some("low")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Medium), Some("gemini-future")),
            Some("medium")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::High), Some("gemini-future")),
            Some("high")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Max), Some("gemini-future")),
            Some("high")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultra), Some("gemini-future")),
            Some("high")
        );
        // Claude rejects --effort outright.
        assert_eq!(
            to_effort(Some(ReasoningLevel::High), Some("claude-sonnet-4-6")),
            None
        );
        assert_eq!(
            to_effort(
                Some(ReasoningLevel::Medium),
                Some("claude-opus-4-6-thinking")
            ),
            None
        );
        // Suffixed ids already select effort; a leftover picker value must not
        // be forwarded (agy treats a mismatch as a conflict).
        assert_eq!(
            to_effort(Some(ReasoningLevel::Medium), Some("gemini-3.8-flash-high")),
            None
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::High), Some("gemini-3.8-flash-high")),
            None
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::High), Some("gpt-oss-120b-medium")),
            None
        );
        // Default model is gemini-3.8-flash-high — omit --effort when unspecified.
        assert_eq!(to_effort(Some(ReasoningLevel::Medium), None), None);
    }

    #[test]
    fn static_models_contain_defaults_and_flash() {
        let models = static_models();
        assert!(models.iter().any(|m| m.id == "gemini-3.8-flash-high"));
        assert!(models.iter().any(|m| m.id == "gemini-3.7-flash-high"));
        assert!(models.iter().any(|m| m.id == "gemini-3.1-pro-high"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == default_model()));
    }

    #[test]
    fn parse_models_handles_carriage_returns_and_spinners() {
        let sample = "⠋ Fetching available models...\r⠙ Fetching available models...\r\x1b[32mgemini-3.8-flash-high     Gemini 3.8 Flash (High)\x1b[0m\ngemini-3.8-flash-medium   Gemini 3.8 Flash (Medium)\nclaude-sonnet-4-6         Claude Sonnet 4.6 (Thinking)\n";
        let parsed = parse_models(sample);
        assert!(
            parsed
                .iter()
                .any(|m| m.id == "gemini-3.8-flash-high" && m.label == "Gemini 3.8 Flash (High)")
        );
        assert!(parsed.iter().any(|m| m.id == "gemini-3.8-flash-medium"));
        assert!(parsed.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(parsed.iter().any(|m| m.id == default_model()));
        let claude = parsed.iter().find(|m| m.id == "claude-sonnet-4-6").unwrap();
        assert!(
            claude.reasoning_levels.is_empty(),
            "Claude rejects --effort; no picker"
        );
        let flash_high = parsed
            .iter()
            .find(|m| m.id == "gemini-3.8-flash-high")
            .unwrap();
        assert_eq!(flash_high.reasoning_levels, vec![ReasoningLevel::High]);
    }

    #[test]
    fn static_claude_has_no_effort_ladder() {
        let models = static_models();
        let claude = models.iter().find(|m| m.id == "claude-sonnet-4-6").unwrap();
        assert!(claude.reasoning_levels.is_empty());
        let opus = models
            .iter()
            .find(|m| m.id == "claude-opus-4-6-thinking")
            .unwrap();
        assert!(opus.reasoning_levels.is_empty());
        let pro_high = models
            .iter()
            .find(|m| m.id == "gemini-3.1-pro-high")
            .unwrap();
        assert_eq!(pro_high.reasoning_levels, vec![ReasoningLevel::High]);
    }
}
