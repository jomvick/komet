use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const API_BASE_URL: &str = "https://backend.composio.dev/api/v3.1";
const PAGE_SIZE: u16 = 100;

#[derive(Clone)]
pub struct ComposioApi {
    client: reqwest::Client,
    api_key: Arc<str>,
    user_id: Arc<str>,
}

impl fmt::Debug for ComposioApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposioApi")
            .field("api_key", &"******")
            .field("user_id", &self.user_id)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposioToolkit {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub logo: Option<String>,
    pub no_auth: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposioAccount {
    pub id: String,
    pub toolkit_slug: String,
    pub status: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ComposioTool {
    pub slug: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct ComposioConnectionLink {
    pub url: String,
    pub connected_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct Page<T> {
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolkitRaw {
    slug: String,
    name: String,
    #[serde(default)]
    no_auth: bool,
    #[serde(default)]
    meta: Option<ToolkitMetadata>,
}

#[derive(Debug, Deserialize)]
struct ToolkitMetadata {
    #[serde(default)]
    description: String,
    #[serde(default)]
    logo: Option<String>,
}

#[derive(Deserialize)]
struct AccountRaw {
    id: String,
    status: String,
    toolkit: AccountToolkit,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    word_id: Option<String>,
    #[serde(default)]
    state: Option<AccountState>,
}

#[derive(Debug, Deserialize)]
struct AccountToolkit {
    slug: String,
}

#[derive(Deserialize)]
struct AccountState {
    #[serde(default)]
    val: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SessionRaw {
    session_id: String,
}

#[derive(Deserialize)]
struct LinkRaw {
    redirect_url: String,
    #[serde(default)]
    connected_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolRaw {
    slug: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    input_parameters: serde_json::Value,
}

impl ComposioApi {
    pub fn new(api_key: String, user_id: String) -> anyhow::Result<Self> {
        if api_key.trim().is_empty() {
            anyhow::bail!("Composio Platform API key is empty");
        }
        if user_id.trim().is_empty() {
            anyhow::bail!("Composio user id is empty");
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            api_key: Arc::from(api_key),
            user_id: Arc::from(user_id),
        })
    }

    pub async fn validate_key(&self) -> anyhow::Result<()> {
        self.get::<Page<ToolkitRaw>>("/toolkits", &[("limit", "1")])
            .await
            .map(|_| ())
    }

    pub async fn search_toolkits(&self, query: &str) -> anyhow::Result<Vec<ComposioToolkit>> {
        let response = self
            .get::<Page<ToolkitRaw>>(
                "/toolkits",
                &[
                    ("search", query),
                    ("limit", &PAGE_SIZE.to_string()),
                    ("sort_by", "usage"),
                ],
            )
            .await?;
        Ok(response
            .items
            .into_iter()
            .map(|toolkit| ComposioToolkit {
                slug: toolkit.slug,
                name: toolkit.name,
                description: toolkit
                    .meta
                    .as_ref()
                    .map(|meta| meta.description.clone())
                    .unwrap_or_default(),
                logo: toolkit.meta.and_then(|meta| meta.logo),
                no_auth: toolkit.no_auth,
            })
            .collect())
    }

    pub async fn connected_accounts(&self) -> anyhow::Result<Vec<ComposioAccount>> {
        let mut cursor: Option<String> = None;
        let mut accounts = Vec::new();
        for _ in 0..50 {
            let mut query = vec![
                ("user_ids", self.user_id.to_string()),
                ("limit", PAGE_SIZE.to_string()),
            ];
            if let Some(cursor_value) = cursor.as_deref() {
                query.push(("cursor", cursor_value.to_string()));
            }
            let query_refs: Vec<_> = query
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            let page = self
                .get::<Page<AccountRaw>>("/connected_accounts", &query_refs)
                .await?;
            accounts.extend(page.items.into_iter().map(|account| {
                let label = account
                    .alias
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| {
                        account.state.and_then(|state| state.val).and_then(|value| {
                            value
                                .get("displayName")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                    })
                    .or(account.word_id);
                ComposioAccount {
                    id: account.id,
                    toolkit_slug: account.toolkit.slug,
                    status: account.status,
                    label,
                }
            }));
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(accounts);
            }
        }
        anyhow::bail!("Composio account list exceeded the pagination limit");
    }

    pub async fn connect(&self, toolkit_slug: &str) -> anyhow::Result<ComposioConnectionLink> {
        validate_slug(toolkit_slug)?;
        let session = self.create_session().await?;
        let response: LinkRaw = self
            .post(
                &format!("/tool_router/session/{}/link", session.session_id),
                &serde_json::json!({ "toolkit": toolkit_slug }),
            )
            .await?;
        validate_connect_url(&response.redirect_url)?;
        Ok(ComposioConnectionLink {
            url: response.redirect_url,
            connected_account_id: response.connected_account_id,
        })
    }

    pub async fn disconnect(&self, account_id: &str) -> anyhow::Result<()> {
        validate_account_id(account_id)?;
        let response = self
            .client
            .delete(format!("{API_BASE_URL}/connected_accounts/{account_id}"))
            .header("x-api-key", self.api_key.as_ref())
            .send()
            .await?;
        ensure_success(response).await
    }

    pub async fn create_session(&self) -> anyhow::Result<ComposioSession> {
        let response: SessionRaw = self
            .post(
                "/tool_router/session",
                &serde_json::json!({
                    "user_id": self.user_id.as_ref(),
                    "manage_connections": { "enable": false },
                    "workbench": { "enable": false },
                    "proxy_execute": { "enable": false },
                    "instant": false
                }),
            )
            .await?;
        if response.session_id.trim().is_empty() {
            anyhow::bail!("Composio returned an empty session id");
        }
        Ok(ComposioSession {
            api: self.clone(),
            session_id: response.session_id,
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        let response = self
            .client
            .get(format!("{API_BASE_URL}{path}"))
            .header("x-api-key", self.api_key.as_ref())
            .query(query)
            .send()
            .await?;
        response_json(response).await
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        let response = self
            .client
            .post(format!("{API_BASE_URL}{path}"))
            .header("x-api-key", self.api_key.as_ref())
            .json(body)
            .send()
            .await?;
        response_json(response).await
    }
}

#[derive(Clone)]
pub struct ComposioSession {
    api: ComposioApi,
    session_id: String,
}

impl fmt::Debug for ComposioSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposioSession")
            .field("session_id", &self.session_id)
            .field("api_key", &"******")
            .finish()
    }
}

impl ComposioSession {
    pub async fn tools(&self) -> anyhow::Result<Vec<ComposioTool>> {
        let mut cursor: Option<String> = None;
        let mut tools = Vec::new();
        for _ in 0..50 {
            let mut query = vec![("limit", PAGE_SIZE.to_string())];
            if let Some(cursor_value) = cursor.as_deref() {
                query.push(("cursor", cursor_value.to_string()));
            }
            let query_refs: Vec<_> = query
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            let page = self
                .api
                .get::<Page<ToolRaw>>(
                    &format!("/tool_router/session/{}/tools", self.session_id),
                    &query_refs,
                )
                .await?;
            tools.extend(page.items.into_iter().map(|tool| ComposioTool {
                slug: tool.slug,
                description: tool.description,
                input_schema: normalize_input_schema(tool.input_parameters),
            }));
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        anyhow::bail!("Composio tool list exceeded the pagination limit");
    }

    pub async fn execute(
        &self,
        tool_slug: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        validate_slug(tool_slug)?;
        self.api
            .post(
                &format!("/tool_router/session/{}/execute", self.session_id),
                &serde_json::json!({
                    "tool_slug": tool_slug,
                    "arguments": arguments
                }),
            )
            .await
    }
}

async fn response_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> anyhow::Result<T> {
    ensure_success_status(response.status())?;
    Ok(response.json().await?)
}

async fn ensure_success(response: reqwest::Response) -> anyhow::Result<()> {
    ensure_success_status(response.status())
}

fn ensure_success_status(status: reqwest::StatusCode) -> anyhow::Result<()> {
    if !status.is_success() {
        anyhow::bail!("Composio API returned HTTP {status}");
    }
    Ok(())
}

fn normalize_input_schema(schema: serde_json::Value) -> serde_json::Value {
    if schema.get("type").and_then(serde_json::Value::as_str) == Some("object") {
        schema
    } else if schema.is_object() {
        serde_json::json!({
            "type": "object",
            "properties": schema,
            "additionalProperties": false
        })
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
}

fn validate_slug(slug: &str) -> anyhow::Result<()> {
    if slug.is_empty()
        || slug.len() > 128
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("Invalid Composio toolkit or tool slug");
    }
    Ok(())
}

fn validate_account_id(account_id: &str) -> anyhow::Result<()> {
    if account_id.is_empty()
        || account_id.len() > 128
        || !account_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("Invalid Composio connected account id");
    }
    Ok(())
}

fn validate_connect_url(raw: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(raw)?;
    if url.scheme() != "https" || url.host_str() != Some("connect.composio.dev") {
        anyhow::bail!("Composio returned an unexpected authorization URL");
    }
    Ok(())
}

pub(crate) fn is_read_action_slug(slug: &str) -> bool {
    const MUTATING_ACTIONS: &[&str] = &[
        "ADD", "APPROVE", "ARCHIVE", "ASSIGN", "CANCEL", "CLOSE", "COMPLETE", "CREATE", "DELETE",
        "EDIT", "INVITE", "MOVE", "PATCH", "POST", "PUBLISH", "PUT", "REMOVE", "REPLY", "SEND",
        "SHARE", "SUBMIT", "TRASH", "UPDATE", "UPLOAD", "WRITE",
    ];
    const READ_ACTIONS: &[&str] = &[
        "CHECK", "COUNT", "FETCH", "FIND", "GET", "LIST", "LOOKUP", "READ", "SEARCH",
    ];
    let segments: Vec<_> = slug
        .split('_')
        .map(|segment| segment.to_ascii_uppercase())
        .collect();
    if segments
        .iter()
        .any(|segment| MUTATING_ACTIONS.contains(&segment.as_str()))
    {
        return false;
    }
    segments
        .iter()
        .any(|segment| READ_ACTIONS.contains(&segment.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_normalization_preserves_object_schemas() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}}
        });
        assert_eq!(normalize_input_schema(schema.clone()), schema);
        for schema in [
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "object", "additionalProperties": true}),
        ] {
            assert_eq!(normalize_input_schema(schema.clone()), schema);
        }
    }

    #[test]
    fn schema_normalization_wraps_parameter_maps() {
        assert_eq!(
            normalize_input_schema(serde_json::json!({"query": {"type": "string"}})),
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn read_action_detection_defaults_unknown_actions_to_mutating() {
        assert!(is_read_action_slug("GMAIL_SEARCH_EMAILS"));
        assert!(is_read_action_slug("SLACK_GET_MESSAGE"));
        assert!(!is_read_action_slug("GMAIL_SEND_EMAIL"));
        assert!(!is_read_action_slug("GMAIL_SEARCH_AND_DELETE_EMAILS"));
        assert!(!is_read_action_slug("CUSTOM_UNKNOWN_ACTION"));
    }

    #[test]
    fn connect_urls_only_allow_composio_https_origin() {
        assert!(validate_connect_url("https://connect.composio.dev/link/ln_123").is_ok());
        assert!(validate_connect_url("http://connect.composio.dev/link/ln_123").is_err());
        assert!(validate_connect_url("https://example.com/link/ln_123").is_err());
    }
}
