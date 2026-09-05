use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

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
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub readonly: bool,
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
}

pub struct McpCatalog {
    tools: HashMap<&'static str, ToolDef>,
    host: Arc<dyn McpHost>,
}

impl McpCatalog {
    pub fn komet_default(host: Arc<dyn McpHost>) -> Self {
        let defs = vec![
            ToolDef {
                name: "list_sessions",
                title: "List sessions",
                description: "List recent chat sessions with id, title and status.",
                input_schema: obj_schema(&[], &[]),
                readonly: true,
            },
            ToolDef {
                name: "get_session_status",
                title: "Get session status",
                description: "Return id, title and status for one session.",
                input_schema: obj_schema(&[("sessionId", "string")], &["sessionId"]),
                readonly: true,
            },
            ToolDef {
                name: "list_spaces",
                title: "List spaces",
                description: "List workspace spaces with id, path and name.",
                input_schema: obj_schema(&[], &[]),
                readonly: true,
            },
        ];
        Self {
            tools: defs.into_iter().map(|d| (d.name, d)).collect(),
            host,
        }
    }

    pub fn list(&self) -> Vec<ToolDef> {
        // Definition order (matches the acceptance test) — NOT sorted.
        ["list_sessions", "get_session_status", "list_spaces"]
            .iter()
            .filter_map(|n| self.tools.get(n).cloned())
            .collect()
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
        match tool.name {
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
            _ => Err(CatalogError::Unknown(name.into())),
        }
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
        let names: Vec<_> = catalog.list().iter().map(|t| t.name.clone()).collect();
        assert_eq!(
            names,
            vec!["list_sessions", "get_session_status", "list_spaces"]
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
}
