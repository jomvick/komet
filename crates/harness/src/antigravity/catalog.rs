//! Model catalog + effort mapping for Google Antigravity CLI (`agy`).
//!
//! Exposes the real models supported by the Antigravity CLI / Gemini runtime,
//! with reasoning ladders and context options.

use komet_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

pub fn default_model() -> &'static str {
    "antigravity-default"
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

pub(crate) const FULL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

pub(crate) const HIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::Max,
];

pub(crate) const STANDARD_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
];

pub(crate) const MINIMAL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
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

/// Map the reasoning level to the `--effort` flag value accepted by `agy`.
pub fn to_effort(level: Option<ReasoningLevel>) -> Option<&'static str> {
    match level? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High | ReasoningLevel::XHigh => Some("high"),
        ReasoningLevel::Max | ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => Some("max"),
    }
}

/// Curated official Antigravity CLI models.
pub fn static_models() -> Vec<Model> {
    vec![
        model(
            "gemini-3.7-flash",
            "Gemini 3.7 Flash",
            "Fast frontier model with hybrid reasoning & coding",
            HIGH_LADDER,
            vec![context_window()],
        ),
        model(
            "gemini-3.7-pro",
            "Gemini 3.7 Pro",
            "Deep multi-step reasoning & architecture agent",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "gemini-2.5-flash",
            "Gemini 2.5 Flash",
            "Lightweight fast everyday model",
            STANDARD_LADDER,
            Vec::new(),
        ),
        model(
            "gemini-2.5-pro",
            "Gemini 2.5 Pro",
            "Advanced reasoning and large-context analysis",
            STANDARD_LADDER,
            vec![context_window()],
        ),
        model(
            "gemini-2.5-flash-lite",
            "Gemini 2.5 Flash Lite",
            "Ultra-fast lightweight model",
            MINIMAL_LADDER,
            Vec::new(),
        ),
        model(
            "claude-3-7-sonnet",
            "Claude 3.7 Sonnet",
            "Anthropic hybrid reasoning model via Antigravity",
            HIGH_LADDER,
            Vec::new(),
        ),
        model(
            default_model(),
            "Antigravity CLI Default",
            "Uses the active model configured in Antigravity CLI",
            STANDARD_LADDER,
            Vec::new(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_mapping_works() {
        assert_eq!(to_effort(None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Low)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::Medium)), Some("medium"));
        assert_eq!(to_effort(Some(ReasoningLevel::High)), Some("high"));
        assert_eq!(to_effort(Some(ReasoningLevel::Max)), Some("max"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultra)), Some("max"));
    }

    #[test]
    fn static_models_contain_defaults_and_flash() {
        let models = static_models();
        assert!(models.iter().any(|m| m.id == "gemini-3.7-flash"));
        assert!(models.iter().any(|m| m.id == "gemini-3.7-pro"));
        assert!(models.iter().any(|m| m.id == default_model()));
    }
}
