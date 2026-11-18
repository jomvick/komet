//! Contexte d'un élément de page capturé par « Add element » : rempli par le
//! script `DescribeAt` (PR 2), émis via [`crate::browser::BrowserSurfaceEvent`]
//! puis indexé dans le composer en bloc `<browser-element>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElementContext {
    pub url: String,
    pub title: String,
    pub tag: String,
    pub role: String,
    /// Texte visible de l'élément, tronqué à 200 caractères côté script.
    pub text: String,
    /// Sélecteur robuste généré côté page (`#id`, `[data-testid]`, ou chemin
    /// `:nth-of-type` validé par `querySelectorAll(...).length === 1`).
    pub selector: String,
    /// Attributs utiles à la ré-identification (id, class, name, href,
    /// aria-label, data-testid, placeholder, type).
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Rectangle dans la page (pixels CSS).
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub width: f32,
    #[serde(default)]
    pub height: f32,
    /// `outerHTML` tronqué (≤ 1500 car.), scripts et styles retirés.
    #[serde(default)]
    pub outer_html: String,
}

/// Rectangle + pastille renvoyés par `probe_at_script` pour le survol du
/// mode sélection (« Add element »).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ElementHover {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Pastille affichée, ex. `button#login · 120×36`.
    #[serde(default)]
    pub label: String,
}

impl ElementContext {
    /// Bloc `<browser-element>` injecté dans le prompt envoyé à l'agent.
    /// Le `outerHTML` prime sur le texte quand il existe.
    pub fn prompt_block(&self) -> String {
        let attr = |name: &str, value: &str| {
            if value.is_empty() {
                String::new()
            } else {
                format!(" {name}=\"{}\"", value.replace('"', "&quot;"))
            }
        };
        let mut block = format!(
            "<browser-element{}{}{}{}{}>",
            attr("url", &self.url),
            attr("title", &self.title),
            attr("selector", &self.selector),
            attr("tag", &self.tag),
            attr("role", &self.role),
        );
        let body = if self.outer_html.is_empty() {
            self.text.as_str()
        } else {
            self.outer_html.as_str()
        };
        block.push('\n');
        block.push_str(body);
        block.push_str("\n</browser-element>");
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_block_serializes_context() {
        let ctx = ElementContext {
            url: "https://example.com".into(),
            title: "Example".into(),
            tag: "button".into(),
            role: "button".into(),
            text: "Login".into(),
            selector: "#login".into(),
            outer_html: "<button id=\"login\">Login</button>".into(),
            ..ElementContext::default()
        };
        let block = ctx.prompt_block();
        assert!(block.starts_with("<browser-element"));
        assert!(block.contains("url=\"https://example.com\""));
        assert!(block.contains("selector=\"#login\""));
        assert!(block.contains("<button id=\"login\">Login</button>"));
        assert!(block.ends_with("</browser-element>"));
    }

    #[test]
    fn prompt_block_falls_back_to_text_and_escapes_quotes() {
        let ctx = ElementContext {
            title: "Say \"hi\"".into(),
            text: "just text".into(),
            ..ElementContext::default()
        };
        let block = ctx.prompt_block();
        assert!(block.contains("title=\"Say &quot;hi&quot;\""));
        assert!(block.contains("\njust text\n"));
        assert!(!block.contains("outer_html"));
    }
}
