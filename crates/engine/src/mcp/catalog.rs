use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::composio::{ComposioSession, ComposioTool, is_read_action_slug};

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpaceSummary {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
}

#[async_trait::async_trait]
pub trait McpHost: Send + Sync {
    fn list_sessions(&self) -> Vec<SessionSummary>;
    fn session_status(&self, id: &str) -> Option<SessionSummary>;
    fn list_spaces(&self) -> Vec<SpaceSummary>;
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub readonly: bool,
    composio_slug: Option<String>,
}

fn obj_schema(props: &[(&str, &str)], required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<_, _> = props
        .iter()
        .map(|(k, t)| (k.to_string(), serde_json::json!({"type": t})))
        .collect();
    serde_json::json!({"type": "object", "properties": properties,
        "required": required, "additionalProperties": false})
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("missing required argument: {0}")]
    MissingArg(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
}

pub struct McpCatalog {
    tools: HashMap<String, ToolDef>,
    order: Vec<String>,
    host: Arc<dyn McpHost>,
    composio: Option<ComposioSession>,
}

impl McpCatalog {
    pub fn komet_default(host: Arc<dyn McpHost>) -> Self {
        let defs = vec![
            ToolDef {
                name: "list_sessions".into(),
                title: "List sessions".into(),
                description: "List recent chat sessions with id, title and status.".into(),
                input_schema: obj_schema(&[], &[]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "get_session_status".into(),
                title: "Get session status".into(),
                description: "Return id, title and status for one session.".into(),
                input_schema: obj_schema(&[("sessionId", "string")], &["sessionId"]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "list_spaces".into(),
                title: "List spaces".into(),
                description: "List workspace spaces with id, path and name.".into(),
                input_schema: obj_schema(&[], &[]),
                readonly: true,
                composio_slug: None,
            },
        ];
        let order = defs
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        Self {
            tools: defs
                .into_iter()
                .map(|definition| (definition.name.clone(), definition))
                .collect(),
            order,
            host,
            composio: None,
        }
    }

    pub fn with_composio(mut self, session: ComposioSession, tools: Vec<ComposioTool>) -> Self {
        self.composio = Some(session);
        for tool in tools {
            let name = format!("composio_{}", tool.slug.to_ascii_lowercase());
            if self.tools.contains_key(&name) {
                tracing::warn!(tool = %tool.slug, "duplicate Composio MCP tool skipped");
                continue;
            }
            self.order.push(name.clone());
            self.tools.insert(
                name.clone(),
                ToolDef {
                    title: tool.slug.clone(),
                    description: tool.description,
                    input_schema: tool.input_schema,
                    readonly: is_read_action_slug(&tool.slug),
                    composio_slug: Some(tool.slug),
                    name,
                },
            );
        }
        self
    }

    pub fn list(&self) -> Vec<ToolDef> {
        // Definition order (matches the acceptance test) — NOT sorted.
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name).cloned())
            .collect()
    }

    pub fn is_readonly(&self, name: &str, arguments: &serde_json::Value) -> bool {
        if name.eq_ignore_ascii_case("composio_composio_multi_execute_tool")
            || name.eq_ignore_ascii_case("composio_multi_execute_tool")
        {
            let mut slugs = Vec::new();
            collect_tool_slugs(arguments, &mut slugs);
            return !slugs.is_empty() && slugs.iter().all(|slug| is_read_action_slug(slug));
        }
        self.tools
            .get(name)
            .is_some_and(|definition| definition.readonly)
    }

    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, CatalogError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| CatalogError::Unknown(name.into()))?;
        match tool.name.as_str() {
            "list_sessions" => Ok(serde_json::json!({"sessions": self.host.list_sessions()})),
            "get_session_status" => {
                let id = input
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| CatalogError::MissingArg("sessionId".into()))?;
                Ok(self
                    .host
                    .session_status(id)
                    .map(|s| serde_json::json!({"session": s}))
                    .unwrap_or_else(|| serde_json::json!({"session": null})))
            }
            "list_spaces" => Ok(serde_json::json!({"spaces": self.host.list_spaces()})),
            _ => match (&tool.composio_slug, &self.composio) {
                (Some(slug), Some(session)) => session
                    .execute(slug, input)
                    .await
                    .map_err(|error| CatalogError::ExecutionFailed(error.to_string())),
                _ => Err(CatalogError::Unknown(name.into())),
            },
        }
    }
}

fn collect_tool_slugs(value: &serde_json::Value, output: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, nested) in object {
                if matches!(key.as_str(), "tool_slug" | "toolSlug" | "slug") {
                    if let Some(slug) = nested.as_str() {
                        output.push(slug.to_string());
                    }
                } else {
                    collect_tool_slugs(nested, output);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_tool_slugs(item, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeHost;

    impl McpHost for FakeHost {
        fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }

        fn session_status(&self, _id: &str) -> Option<SessionSummary> {
            None
        }

        fn list_spaces(&self) -> Vec<SpaceSummary> {
            vec![SpaceSummary {
                id: "s1".into(),
                path: "/r".into(),
                name: None,
            }]
        }
    }

    #[tokio::test]
    async fn catalog_lists_three_readonly_tools_and_executes() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        let names: Vec<_> = catalog.list().into_iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            vec![
                "list_sessions".to_string(),
                "get_session_status".to_string(),
                "list_spaces".to_string()
            ]
        );
        assert!(catalog.list().iter().all(|t| t.readonly));
        let v = catalog
            .execute("list_spaces", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"spaces": [{"id": "s1", "path": "/r", "name": null}]})
        );
        assert!(
            catalog
                .execute("nope", serde_json::json!({}))
                .await
                .is_err()
        );
    }

    #[test]
    fn composio_mutations_require_permission_but_reads_do_not() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        assert!(catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": [{"tool_slug": "GMAIL_SEARCH_EMAILS"}]})
        ));
        assert!(!catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": [{"tool_slug": "GMAIL_SEND_EMAIL"}]})
        ));
        assert!(!catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": []})
        ));
    }
}
