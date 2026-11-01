use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, watch};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use super::catalog::McpCatalog;
use super::policy::{Decision, McpPolicy, secure_summary};
pub use crate::mcp::status::McpStatus;

const MCP_PATH: &str = "/mcp/agents";
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_CALLS: usize = 4;
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(60);

pub struct PermissionAsk {
    pub server: String,
    pub tool: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskDecision {
    Allow,
    AllowSession,
    Deny,
}

pub type AskFn = Arc<dyn Fn(PermissionAsk) -> BoxFuture<'static, AskDecision> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("failed to bind MCP endpoint: {0}")]
    Bind(#[from] std::io::Error),
}

struct EndpointState {
    status: Mutex<McpStatus>,
    watch_tx: watch::Sender<McpStatus>,
    watch_rx: watch::Receiver<McpStatus>,
}

impl EndpointState {
    fn new(initial: McpStatus) -> Self {
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            status: Mutex::new(initial),
            watch_tx: tx,
            watch_rx: rx,
        }
    }
}

#[derive(Clone)]
struct AuthState {
    run_id: Arc<str>,
    token_hash: [u8; 32],
}

pub struct RunningEndpoint {
    url: String,
    token: String,
    cancellation: CancellationToken,
    abort: AbortHandle,
    state: Arc<EndpointState>,
}

impl RunningEndpoint {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn status(&self) -> McpStatus {
        self.state
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<McpStatus> {
        self.state.watch_rx.clone()
    }

    pub fn shutdown(&self) {
        self.cancellation.cancel();
        self.abort.abort();
        set_status(&self.state, McpStatus::Stopped);
    }

    pub(crate) fn auth_token(&self) -> &str {
        &self.token
    }

    #[cfg(test)]
    fn token_for_test_only(&self) -> &str {
        self.auth_token()
    }
}

impl Drop for RunningEndpoint {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct McpEndpoint;

impl McpEndpoint {
    pub async fn start(
        run_id: &str,
        catalog: Arc<McpCatalog>,
        policy: McpPolicy,
        ask: AskFn,
    ) -> Result<RunningEndpoint, EndpointError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().to_string();
        let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let state = Arc::new(EndpointState::new(McpStatus::Starting));
        let cancellation = CancellationToken::new();
        let policy = Arc::new(Mutex::new(policy));
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CALLS));
        let auth = AuthState {
            run_id: Arc::from(run_id.to_owned()),
            token_hash,
        };
        let task_state = Arc::clone(&state);
        let task_cancellation = cancellation.clone();
        let task_catalog = Arc::clone(&catalog);
        let task = tokio::spawn(async move {
            set_status(&task_state, McpStatus::Ready);
            loop {
                let cancel = task_cancellation.clone();
                let accept = tokio::select! {
                    _ = cancel.cancelled() => break,
                    res = listener.accept() => res,
                };
                let (stream, _) = match accept {
                    Ok(v) => v,
                    Err(e) => {
                        set_status(&task_state, McpStatus::Error(e.to_string()));
                        break;
                    }
                };
                let state = Arc::clone(&task_state);
                let auth = auth.clone();
                let catalog = Arc::clone(&task_catalog);
                let policy = Arc::clone(&policy);
                let semaphore = Arc::clone(&semaphore);
                let ask = Arc::clone(&ask);
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, auth, catalog, policy, semaphore, ask).await
                    {
                        tracing::debug!(error=%e, "mcp connection error (masked)");
                    }
                    drop(state);
                });
            }
            if !matches!(get_status(&task_state), McpStatus::Stopped) {
                // keep as is if already stopped
            }
        });
        // Ensure ready
        set_status(&state, McpStatus::Ready);
        Ok(RunningEndpoint {
            url: format!("http://{address}{MCP_PATH}?callerAgentId={run_id}"),
            token,
            cancellation,
            abort: task.abort_handle(),
            state,
        })
    }
}

async fn handle_conn(
    mut stream: tokio::net::TcpStream,
    auth: AuthState,
    catalog: Arc<McpCatalog>,
    policy: Arc<Mutex<McpPolicy>>,
    semaphore: Arc<Semaphore>,
    ask: AskFn,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req_str = String::from_utf8_lossy(&buf[..n]).to_string();
    // Parse request line and headers
    let mut lines = req_str.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut content_length: usize = 0;
    let mut auth_header: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
                if content_length > MAX_REQUEST_BYTES {
                    content_length = MAX_REQUEST_BYTES;
                }
            }
            if key == "authorization" {
                auth_header = Some(val.clone());
            }
        }
    }
    // Extract path+query from request line: "POST /mcp/agents?callerAgentId=... HTTP/1.1"
    let path_query = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_query, None),
    };
    // Handle body – if content_length > already read body part, read more
    let header_end = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
    let mut body = if n > header_end {
        buf[header_end..n].to_vec()
    } else {
        Vec::new()
    };
    let remaining = content_length.saturating_sub(body.len());
    if remaining > 0 {
        let mut extra = vec![0u8; remaining];
        // read with timeout
        let read_n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut extra))
            .await
            .unwrap_or(Ok(0))
            .unwrap_or(0);
        body.extend_from_slice(&extra[..read_n]);
        if body.len() > MAX_REQUEST_BYTES {
            body.truncate(MAX_REQUEST_BYTES);
        }
    }
    let body_str = String::from_utf8_lossy(&body).to_string();

    // Auth check
    let token_ok = auth_header
        .as_deref()
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| {
            let h: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            h == auth.token_hash
        })
        .unwrap_or(false);
    if !token_ok {
        let resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }
    if query_value(query, "callerAgentId") != Some(auth.run_id.as_ref()) {
        let resp = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }
    if path != MCP_PATH {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    // Parse JSON-RPC
    let json: serde_json::Value =
        serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
    let method = json.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let params = json
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let (result, _is_error) = match method {
        "tools/list" => {
            let tools: Vec<serde_json::Value> = catalog
                .list()
                .into_iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.name,
                        "title": d.title,
                        "description": d.description,
                        "inputSchema": d.input_schema,
                        "annotations": {"readOnly": d.readonly}
                    })
                })
                .collect();
            (serde_json::json!({"tools": tools}), false)
        }
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            // policy check
            let definition = catalog.list().into_iter().find(|t| t.name == tool_name);
            let readonly = definition.is_some() && catalog.is_readonly(&tool_name, &arguments);
            let decision = policy
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .decide("komet", &tool_name, readonly);
            let allowed = match decision {
                Decision::Allow => true,
                Decision::Deny => false,
                Decision::Ask => {
                    let summary = secure_summary("komet", &tool_name, &arguments);
                    let ask_res = tokio::time::timeout(
                        PERMISSION_TIMEOUT,
                        (ask)(PermissionAsk {
                            server: "komet".to_string(),
                            tool: tool_name.clone(),
                            summary,
                        }),
                    )
                    .await
                    .unwrap_or(AskDecision::Deny);
                    match ask_res {
                        AskDecision::Allow => true,
                        AskDecision::AllowSession => {
                            policy.lock().unwrap_or_else(|p| p.into_inner()).memoize(
                                "komet",
                                &tool_name,
                                Decision::Allow,
                            );
                            true
                        }
                        AskDecision::Deny => false,
                    }
                }
            };
            if !allowed {
                let err = format!("tool denied by policy: {tool_name}");
                (
                    serde_json::json!({"content": [{"type":"text","text": err}], "isError": true}),
                    true,
                )
            } else {
                // execute with semaphore and timeout
                let permit = semaphore.acquire().await;
                let permit_ok = permit.is_ok();
                let exec_res = if permit_ok {
                    let _permit = permit.unwrap();
                    tokio::time::timeout(TOOL_TIMEOUT, catalog.execute(&tool_name, arguments))
                        .await
                        .map_err(|_| "MCP tool execution timed out".to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                } else {
                    Err("MCP endpoint is shutting down".to_string())
                };
                match exec_res {
                    Ok(v) => (
                        serde_json::json!({"content": [{"type":"text","text": serde_json::to_string(&v).unwrap_or_default()}], "structuredContent": v}),
                        false,
                    ),
                    Err(e) => (
                        serde_json::json!({"content": [{"type":"text","text": e}], "isError": true}),
                        true,
                    ),
                }
            }
        }
        "initialize" => (
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "komet", "version": env!("CARGO_PKG_VERSION")}
            }),
            false,
        ),
        _ => (serde_json::json!({"error": "unknown method"}), true),
    };

    let response_body = serde_json::json!({"jsonrpc":"2.0","id": id, "result": result});
    let body_bytes = serde_json::to_vec(&response_body).unwrap();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body_bytes.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(&body_bytes).await?;
    Ok(())
}

fn query_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|part| {
        let (candidate, value) = part.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

fn set_status(state: &EndpointState, status: McpStatus) {
    *state
        .status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = status.clone();
    let _ = state.watch_tx.send(status);
}

fn get_status(state: &EndpointState) -> McpStatus {
    state
        .status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::mcp::catalog::McpCatalog;
    use crate::mcp::catalog::{McpHost, SessionSummary, SpaceSummary, TerminalAngle, TerminalRead};
    use crate::mcp::policy::McpPolicy;
    use komet_proto::{TerminalInfo, TerminalRunResult};

    /// A host that records EVERY terminal write it is asked for. The
    /// permission tests below assert the counter stays at zero when the human
    /// did not approve — the only way to prove a denied `tools/call` executed
    /// nothing rather than "executed then reported a denial".
    #[derive(Default)]
    struct FakeHost {
        write_calls: AtomicUsize,
        asked_tools: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHost {
        fn writes(&self) -> usize {
            self.write_calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl McpHost for FakeHost {
        fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }

        fn session_status(&self, _id: &str) -> Option<SessionSummary> {
            None
        }

        fn list_spaces(&self) -> Vec<SpaceSummary> {
            vec![]
        }

        fn list_terminals_for_agents(&self) -> Vec<TerminalInfo> {
            vec![]
        }

        fn read_terminal_for_agents(
            &self,
            _terminal_id: &str,
            _angle: &TerminalAngle,
        ) -> Result<TerminalRead, String> {
            Err("terminal reads are not wired in this test host".to_string())
        }

        fn open_terminal_for_agent(&self, _cwd: Option<String>) -> Result<TerminalInfo, String> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            Err("no terminal openings in this test host".to_string())
        }

        fn close_terminal_for_agent(&self, _terminal_id: &str) -> Result<(), String> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            Err("no terminal closings in this test host".to_string())
        }

        fn send_input_to_terminal(&self, _terminal_id: &str, _bytes: &[u8]) -> Result<(), String> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            Err("no terminal input in this test host".to_string())
        }

        async fn run_in_terminal(
            &self,
            _terminal_id: &str,
            _command: &str,
            _wait_ms: u64,
        ) -> Result<TerminalRunResult, String> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            Err("no terminal runs in this test host".to_string())
        }
    }

    /// An endpoint whose permission prompt answers `answer` for every tool and
    /// records the tools it was asked about, on the given host.
    #[allow(clippy::redundant_clone)] // the `asked` clone hands the future a 'static handle
    async fn start_endpoint_with(host: Arc<FakeHost>, answer: AskDecision) -> RunningEndpoint {
        let asked = host.asked_tools.clone();
        McpEndpoint::start(
            "run-test",
            Arc::new(McpCatalog::komet_default(host)),
            McpPolicy::komet_default(),
            Arc::new(move |ask: PermissionAsk| {
                let asked = asked.clone();
                Box::pin(async move {
                    asked.lock().unwrap().push(ask.tool);
                    answer
                })
            }),
        )
        .await
        .unwrap()
    }

    async fn start_test_endpoint() -> RunningEndpoint {
        start_endpoint_with(Arc::new(FakeHost::default()), AskDecision::Deny).await
    }

    async fn post(
        endpoint: &RunningEndpoint,
        method: &str,
        params: serde_json::Value,
        token: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let client = reqwest::Client::new();
        let mut request = client
            .post(endpoint.url())
            .header("Accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }));
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn endpoint_serves_tools_list_with_bearer() {
        let endpoint = start_test_endpoint().await;
        let token = endpoint.token_for_test_only().to_string();
        let (status, body) =
            post(&endpoint, "tools/list", serde_json::json!({}), Some(&token)).await;
        assert_eq!(status, 200);
        // Compare names to what THIS endpoint's catalog declares, so the wire
        // list cannot drift from the catalog and so adding a tool does not
        // silently rot a hard-coded count.
        let served: Vec<String> = body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("tool name").to_string())
            .collect();
        let declared: Vec<String> = McpCatalog::komet_default(Arc::new(FakeHost::default()))
            .list()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(served, declared);
        assert!(!served.is_empty(), "endpoint must serve at least one tool");
        endpoint.shutdown();
    }

    /// The end-to-end proof of the write door: an agent's `terminal_run` over
    /// the wire reaches the PTY ONLY when a human said yes. Denied → the host
    /// was asked for permission and executed NOTHING (write counter 0, error
    /// text says denied). Approved → exactly one execution. Reads, in the same
    /// test, never trigger the prompt at all — the two doors stay separate.
    #[tokio::test]
    async fn a_denied_write_tool_call_executes_nothing_and_an_approved_one_runs_once() {
        for (answer, expected_writes) in [(AskDecision::Deny, 0), (AskDecision::Allow, 1)] {
            let host = Arc::new(FakeHost::default());
            let endpoint = start_endpoint_with(host.clone(), answer).await;
            let token = endpoint.token_for_test_only().to_string();
            let (status, body) = post(
                &endpoint,
                "tools/call",
                serde_json::json!({
                    "name": "terminal_run",
                    "arguments": {"terminalId": "t-human", "command": "rm -rf /"},
                }),
                Some(&token),
            )
            .await;
            assert_eq!(
                status, 200,
                "the denial is a tool result, not an HTTP failure"
            );
            let text = body["result"]["content"][0]["text"]
                .as_str()
                .expect("content text")
                .to_string();
            if expected_writes == 0 {
                assert!(
                    text.contains("tool denied by policy"),
                    "a denial must be said out loud: {text}"
                );
                assert_eq!(body["result"]["isError"], serde_json::json!(true));
                assert_eq!(
                    host.writes(),
                    0,
                    "an unapproved write must not have touched the engine"
                );
                assert_eq!(
                    *host.asked_tools.lock().unwrap(),
                    vec!["terminal_run".to_string()],
                    "the human was asked, once, before anything ran"
                );
            } else {
                // Approved: the engine WAS consulted (the test host then
                // refuses on its own; counting the consultation is the point).
                assert_eq!(host.writes(), 1, "the approved call ran exactly once");
                assert_eq!(
                    *host.asked_tools.lock().unwrap(),
                    vec!["terminal_run".to_string()]
                );
                assert!(text.contains("no terminal runs"), "{text}");
            }
            endpoint.shutdown();
        }

        // The read door: an auto-allowed read never asks a human, and never
        // counts as a write.
        let host = Arc::new(FakeHost::default());
        let endpoint = start_endpoint_with(host.clone(), AskDecision::Deny).await;
        let token = endpoint.token_for_test_only().to_string();
        let (_, body) = post(
            &endpoint,
            "tools/call",
            serde_json::json!({
                "name": "terminal_read",
                "arguments": {"terminalId": "t-visible", "angle": "tail"},
            }),
            Some(&token),
        )
        .await;
        assert_eq!(
            *host.asked_tools.lock().unwrap(),
            Vec::<String>::new(),
            "a readonly tool must not burn a human approval"
        );
        assert_eq!(host.writes(), 0);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not wired in this test host"), "{text}");
        endpoint.shutdown();
    }

    /// Every write tool goes through the prompt: a policy that auto-allows
    /// readonly tools must answer `Ask` for all four, so none can slip past a
    /// human by being mislabelled.
    #[tokio::test]
    async fn every_write_tool_asks_before_running() {
        let host = Arc::new(FakeHost::default());
        let endpoint = start_endpoint_with(host.clone(), AskDecision::Deny).await;
        let token = endpoint.token_for_test_only().to_string();
        let calls = [
            ("terminal_open", serde_json::json!({"cwd": "/tmp"})),
            ("terminal_close", serde_json::json!({"terminalId": "t-1"})),
            (
                "terminal_send_input",
                serde_json::json!({"terminalId": "t-1", "keys": "ctrl+c"}),
            ),
            (
                "terminal_run",
                serde_json::json!({"terminalId": "t-1", "command": "ls"}),
            ),
        ];
        for (name, arguments) in calls {
            let (status, body) = post(
                &endpoint,
                "tools/call",
                serde_json::json!({"name": name, "arguments": arguments}),
                Some(&token),
            )
            .await;
            assert_eq!(status, 200);
            let text = body["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(
                text,
                format!("tool denied by policy: {name}"),
                "{name} must be denied before execution"
            );
        }
        assert_eq!(
            *host.asked_tools.lock().unwrap(),
            vec![
                "terminal_open",
                "terminal_close",
                "terminal_send_input",
                "terminal_run"
            ],
            "each write tool is approved on its own call, in order"
        );
        assert_eq!(host.writes(), 0, "four denials, zero executions");
        endpoint.shutdown();
    }

    #[tokio::test]
    async fn endpoint_rejects_bad_bearer_and_redacts_token_in_status() {
        let endpoint = start_test_endpoint().await;
        let token = endpoint.token_for_test_only().to_string();
        let (status, _) = post(&endpoint, "tools/list", serde_json::json!({}), None).await;
        assert_eq!(status, 401);
        let (status, _) = post(
            &endpoint,
            "tools/list",
            serde_json::json!({}),
            Some("wrong"),
        )
        .await;
        assert_eq!(status, 401);
        assert!(!format!("{:?}", endpoint.status()).contains(&token));
        endpoint.shutdown();
    }

    #[tokio::test]
    async fn endpoint_rejects_a_token_replayed_for_another_run() {
        let endpoint = start_test_endpoint().await;
        let token = endpoint.token_for_test_only().to_string();
        let url = endpoint
            .url()
            .replace("callerAgentId=run-test", "callerAgentId=other-run");
        let response = reqwest::Client::new()
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        endpoint.shutdown();
    }

    #[tokio::test]
    async fn status_watch_transitions() {
        let ep = start_test_endpoint().await;
        assert_eq!(ep.status(), McpStatus::Ready);
        let mut rx = ep.watch_status();
        assert_eq!(*rx.borrow(), McpStatus::Ready);
        ep.shutdown();
        assert_eq!(ep.status(), McpStatus::Stopped);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), McpStatus::Stopped);
    }

    #[tokio::test]
    async fn run_endpoint_lifecycle_ready_then_stopped_with_rotation() {
        let first = start_test_endpoint().await;
        assert_eq!(first.status(), McpStatus::Ready);
        assert!(first.url().starts_with("http://127.0.0.1:"));
        let token1 = first.token_for_test_only().to_string();
        first.shutdown();
        assert_eq!(first.status(), McpStatus::Stopped);
        let second = start_test_endpoint().await;
        assert_ne!(second.token_for_test_only(), token1);
        second.shutdown();
    }
}
