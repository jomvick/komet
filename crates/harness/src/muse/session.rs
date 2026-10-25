//! One Muse session over one host: open (or resume) it, run its turns, and
//! carry Komet's steering, stops and answers in.
//!
//! - TURNS. A fresh turn's id is its `turn/start` command id, so the run
//!   knows it before the ack and owns every frame naming it. Its end is the
//!   host's `turn/completed` — authoritative, also for the wake turns Muse
//!   starts on its own when background work finishes (`turn/started` for a
//!   turn nobody submitted).
//! - STEERING. Every later prompt is `turn/start` with `ifBusy: "steer"`:
//!   one atomic command that steers the running turn or starts one when
//!   idle, so a prompt can never fall between the two. Each is confirmed
//!   (`Steered`) when its user-message item enters the conversation.
//! - STOP. `turn/interrupt` ends the turn in place (`cancelled`), leaving
//!   the host and its background subagents running.
//! - APPROVALS pass through Komet's permission bridge.
//!   QUESTIONS go to the user through [`RunControls::request_input`].
use super::{
    catalog,
    host::{Host, failed},
    normalize::{Normalizer, string, terminal},
    rpc::{Client, Inbound, command_id},
};
use crate::{HarnessError, SteerMessage};
use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, UserInputAnswer, UserInputQuestion,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinSet},
};

pub(super) type Input =
    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;
pub(super) type Permission = dyn Fn(
        komet_proto::PermissionKind,
        String,
        Vec<komet_proto::PermissionChoice>,
    ) -> oneshot::Receiver<komet_proto::PermissionChoice>
    + Send
    + Sync;

/// Anthropic-style caps on inline images apply to Muse too.
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);

/// What a request in flight was for.
enum Pending {
    /// A prompt's `turn/start`; `turn` is the id it would start.
    Prompt { turn: String },
    /// Anything else: only a refusal matters.
    Other(&'static str),
}

pub(super) struct Runner {
    pub host: Host,
    client: Client,
    pub tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request: RunRequest,
    read_only: bool,
    session: String,
    root: String,
    assistant: String,
    norm: Normalizer,
    /// The turn this run is in, and turns joined into it.
    active: Option<String>,
    joined: HashSet<String>,
    /// The first prompt was sent and its turn has not ended.
    first: bool,
    interrupting: bool,
    pub error: Option<String>,
    pending: HashMap<u64, Pending>,
    /// Prompts accepted but not yet seen entering the conversation.
    deliveries: VecDeque<()>,
    /// The first user message of the opening prompt is not a delivery.
    initial_message: bool,
    seen_messages: HashSet<String>,
    pub input: Option<Arc<Input>>,
    pub permission: Option<Arc<Permission>>,
    dialogs: JoinSet<()>,
    questions: HashMap<String, AbortHandle>,
    lost_context: Option<String>,
}

impl Runner {
    pub fn new(
        host: Host,
        tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
        request: RunRequest,
        read_only: bool,
    ) -> Self {
        let client = host.client();
        let root = workspace_root(Path::new(&request.cwd));
        Self {
            host,
            client,
            tx,
            request,
            read_only,
            session: String::new(),
            root,
            assistant: uuid::Uuid::new_v4().to_string(),
            norm: Normalizer::default(),
            active: None,
            joined: HashSet::new(),
            first: false,
            interrupting: false,
            error: None,
            pending: HashMap::new(),
            deliveries: VecDeque::new(),
            initial_message: true,
            seen_messages: HashSet::new(),
            input: None,
            permission: None,
            dialogs: JoinSet::new(),
            questions: HashMap::new(),
            lost_context: None,
        }
    }

    async fn emit(&self, event: AgentEvent) -> Result<(), HarnessError> {
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| HarnessError::Protocol("Muse event consumer closed".into()))
    }

    async fn started(&self, model: String) -> Result<(), HarnessError> {
        self.emit(AgentEvent::SessionStarted {
            harness: HarnessId::Muse,
            model,
            tools: vec![],
            cwd: self.request.cwd.clone(),
            session_id: self.session.clone(),
            assistant_message_id: self.assistant.clone(),
        })
        .await
    }

    /// The run's last word for an unfinished turn (teardown, crash).
    pub async fn finish(&mut self) -> Result<(), HarnessError> {
        if self.active.is_none() && !self.first {
            return Ok(());
        }
        self.active = None;
        self.first = false;
        self.dialogs.abort_all();
        let status = if self.error.is_some() {
            DoneStatus::Errored
        } else {
            DoneStatus::Interrupted
        };
        self.emit(AgentEvent::Done {
            status,
            result: None,
            error: self.error.clone(),
            session_id: (!self.session.is_empty()).then(|| self.session.clone()),
            reason: None,
        })
        .await
    }

    fn model(&self) -> Option<&str> {
        self.request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty() && *m != "default")
    }

    fn approval_mode(&self) -> &'static str {
        if self.read_only {
            "denyUnmatched"
        } else if self.request.auto_approve {
            "allowAll"
        } else {
            "promptUnmatched"
        }
    }

    /// Without an explicit profile Muse applies its configured default, which
    /// can be an AI reviewer deciding approvals instead of the run.
    fn permission_profile(&self) -> &'static str {
        if self.request.auto_approve {
            ":unrestricted"
        } else {
            ":ask-me"
        }
    }

    /// Open the session: resume the stored one, or start fresh — also when
    /// the stored one is gone, with a notice (the engine would otherwise try
    /// the same dead id forever).
    async fn open(&mut self, backlog: &mut Vec<Inbound>) -> Result<Value, HarnessError> {
        // Experimental: `usage/changed` (the subscription meters, observed
        // from Meta's model stream) is delivered only on opted-in connections.
        self.host.initialize(self.read_only).await?;
        if self.request.mcp.is_some() || !self.request.mcp_external.is_empty() {
            self.lost_context = Some(
                "Muse does not currently support Komet's HTTP MCP endpoint; Komet tools are unavailable for this run.".into(),
            );
        }
        if let Some(id) = self.request.resume.clone() {
            let params = json!({
                "sessionId": id,
                "excludeItems": true,
                "permissionProfile": self.permission_profile(),
            });
            match self.host.call("session/resume", params, backlog).await? {
                Ok(result) => {
                    self.session = id;
                    let mode = json!({"sessionId": self.session, "mode": self.approval_mode()});
                    self.host
                        .call("session/setApprovalMode", mode, backlog)
                        .await?
                        .map_err(|f| failed("session/setApprovalMode", &f))?;
                    return Ok(result);
                }
                Err(failure) if matches!(failure.kind.as_str(), "sessionNotFound" | "notFound") => {
                    self.lost_context = Some(format!(
                        "Muse could not restore session {id}; starting a new session without the \
                         previous context: {}",
                        failure.message
                    ));
                    self.request.resume = None;
                }
                Err(failure) => return Err(failed("session/resume", &failure)),
            }
        }
        self.session = command_id();
        let mut params = json!({
            "sessionId": self.session,
            "workspaceRoot": self.root,
            "providerId": "meta",
            "approvalMode": self.approval_mode(),
        });
        if !self.read_only {
            params["permissionProfile"] = json!(self.permission_profile());
        }
        if let Some(model) = self.model() {
            params["modelId"] = json!(model);
        }
        let result = self
            .host
            .call("session/start", params, backlog)
            .await?
            .map_err(|f| failed("session/start", &f))?;
        if result["session"]["sessionId"] != self.session.as_str() {
            return Err(HarnessError::Protocol(
                "Muse started a different session than it was asked to".into(),
            ));
        }
        Ok(result)
    }

    /// Run the opening prompt, then every steer, until steering closes and
    /// the last turn has ended.
    pub async fn run(
        &mut self,
        steering: &mut mpsc::Receiver<SteerMessage>,
    ) -> Result<(), HarnessError> {
        let mut backlog = Vec::new();
        let opened = tokio::time::timeout(OPEN_TIMEOUT, self.open(&mut backlog))
            .await
            .map_err(|_| HarnessError::Protocol("Muse startup timed out".into()))??;
        let mut model = opened["session"]["modelId"].as_str().map(str::to_owned);
        if let Some(wanted) = self.model().map(str::to_owned)
            && model.as_deref() != Some(&wanted)
            && self.request.resume.is_some()
        {
            self.set_model(&wanted)?;
            model = Some(wanted);
        }
        self.started(model.unwrap_or_else(|| "default".into()))
            .await?;
        if let Some(message) = self.lost_context.take() {
            self.emit(AgentEvent::Error {
                message,
                kind: komet_proto::ErrorKind::Unknown,
                detail: None,
                retryable: false,
            })
            .await?;
        }
        let prompt = if self.read_only {
            format!(
                "Please answer this request briefly:\n{}",
                self.request.prompt
            )
        } else {
            self.request.prompt.clone()
        };
        let attachments = self.request.attachments.clone();
        let turn = self.submit(&prompt, &attachments, "queue").await?;
        self.active = Some(turn);
        self.first = true;
        for frame in backlog {
            self.frame(frame).await?;
        }
        let mut open = !self.read_only;
        loop {
            if !open && self.active.is_none() && self.pending.is_empty() {
                return Ok(());
            }
            tokio::select! {
                frame = self.host.next() => self.frame(frame?).await?,
                steer = steering.recv(), if open => match steer {
                    Some(steer) => self.steer(steer).await?,
                    None => open = false,
                },
            }
        }
    }

    fn set_model(&mut self, model: &str) -> Result<(), HarnessError> {
        let id = self.client.request(
            "session/setModel",
            json!({"sessionId": self.session, "model": {"modelId": model, "providerId": "meta"}}),
        )?;
        self.pending.insert(id, Pending::Other("session/setModel"));
        Ok(())
    }

    /// `turn/start` for `prompt`; returns the id a fresh turn would get.
    async fn submit(
        &mut self,
        prompt: &str,
        attachments: &[String],
        if_busy: &str,
    ) -> Result<String, HarnessError> {
        let turn = command_id();
        let mut input = Vec::new();
        if !prompt.is_empty() {
            input.push(json!({"type": "text", "text": prompt}));
        }
        input.extend(load_images(attachments).await);
        if input.is_empty() {
            input.push(json!({"type": "text", "text": "(no message)"}));
        }
        let mut params = json!({
            "sessionId": self.session,
            "commandId": turn,
            "input": input,
            "displayText": if prompt.is_empty() { "Image attachment" } else { prompt },
            "ifBusy": if_busy,
        });
        // A resumed session keeps the root it was created with; pin it to
        // this run's checkout so edits land in the selected workspace. The root
        // is sticky (it binds at this turn and persists), and Muse refuses it
        // on a steer, which may launch no turn to bind it to: the run's first
        // prompt carries it, later ones inherit it.
        if if_busy != "steer" {
            params["workspaceRoots"] = json!([self.root]);
        }
        if let Some(effort) = catalog::effort(self.request.reasoning) {
            params["reasoningEffort"] = json!(effort);
        }
        let id = self.client.request("turn/start", params)?;
        self.pending
            .insert(id, Pending::Prompt { turn: turn.clone() });
        Ok(turn)
    }

    async fn steer(&mut self, steer: SteerMessage) -> Result<(), HarnessError> {
        // A changed model applies on the live session before the prompt it
        // came with (commands run in order); the effort rides the turn.
        let turn = self.submit(&steer.prompt, &[], "steer").await?;
        self.deliveries.push_back(());
        if self.active.is_none() {
            // Idle: this prompt starts the next turn, under its own id.
            self.begin(turn);
        }
        Ok(())
    }

    fn begin(&mut self, turn: String) {
        self.active = Some(turn);
        self.joined.clear();
        self.interrupting = false;
        self.error = None;
        self.norm.reset();
    }

    /// End the turn in place; `turn/completed` (cancelled) ends it for real.
    fn stop(&mut self) {
        let Some(turn) = self.active.clone() else {
            return;
        };
        self.interrupting = true;
        // Muse runs a joined turn first, so a stop must end it too.
        for joined in self.joined.iter().cloned().collect::<Vec<_>>() {
            let _ = self.interrupt_turn(&joined);
        }
        let _ = self.interrupt_turn(&turn);
    }

    fn interrupt_turn(&mut self, turn: &str) -> Result<(), HarnessError> {
        let id = self.client.request(
            "turn/interrupt",
            json!({"sessionId": self.session, "turnId": turn}),
        )?;
        self.pending.insert(id, Pending::Other("turn/interrupt"));
        Ok(())
    }

    /// Runtime teardown: stop the turn, give Muse `grace` to confirm.
    pub async fn interrupt(&mut self, grace: Duration) {
        self.dialogs.abort_all();
        if self.active.is_none() || self.session.is_empty() {
            return;
        }
        self.stop();
        let _ = tokio::time::timeout(grace, async {
            while self.active.is_some() {
                let Ok(frame) = self.host.next().await else {
                    break;
                };
                if self.frame(frame).await.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    fn owns(&self, turn: &str) -> bool {
        self.active.as_deref() == Some(turn) || self.joined.contains(turn)
    }

    async fn confirm_delivery(&mut self) -> Result<(), HarnessError> {
        if self.deliveries.pop_front().is_some() {
            let old = std::mem::replace(&mut self.assistant, uuid::Uuid::new_v4().to_string());
            self.emit(AgentEvent::Steered {
                assistant_message_id: Some(old),
                next_assistant_message_id: Some(self.assistant.clone()),
            })
            .await?;
        }
        Ok(())
    }

    async fn frame(&mut self, frame: Inbound) -> Result<(), HarnessError> {
        match frame {
            Inbound::Response { id, result } => self.response(id, result).await,
            Inbound::Notification { method, params } => self.notification(&method, params).await,
            Inbound::Request { .. } => Ok(()),
        }
    }

    async fn response(
        &mut self,
        id: u64,
        result: Result<Value, super::rpc::Failure>,
    ) -> Result<(), HarnessError> {
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        match (pending, result) {
            (Pending::Prompt { turn }, Ok(ack)) => {
                let absorbed = string(&ack, "turnId").to_owned();
                if absorbed.is_empty() || self.owns(&absorbed) {
                    return Ok(());
                }
                if string(&ack, "disposition") != "queued" && self.active.as_deref() == Some(&turn)
                {
                    // An idle prompt went into a turn that was already running
                    // (a wake not yet announced): that turn is the run's now.
                    self.active = Some(absorbed);
                } else {
                    // Queued behind the running turn: Muse runs it next.
                    self.joined.insert(absorbed);
                }
            }
            (Pending::Prompt { turn }, Err(failure)) => {
                let message = format!("Muse could not take the message: {}", failure.message);
                self.deliveries.pop_front();
                if self.active.as_deref() == Some(&turn) {
                    // The turn this prompt would have started never began.
                    self.error = Some(message);
                    return self.end(DoneStatus::Errored).await;
                }
                self.emit(AgentEvent::Error {
                    message,
                    kind: komet_proto::ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                })
                .await?;
            }
            (Pending::Other(method), Err(failure)) => {
                tracing::warn!(target: "komet_harness::muse", %method, kind = %failure.kind, "{}", failure.message);
                self.emit(AgentEvent::Error {
                    message: format!("Muse {method} failed: {}", failure.message),
                    kind: komet_proto::ErrorKind::Unknown,
                    detail: Some(failure.message),
                    retryable: false,
                })
                .await?;
            }
            (Pending::Other(_), Ok(_)) => {}
        }
        Ok(())
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), HarnessError> {
        if method == "usage/changed" {
            return Ok(());
        }
        if string(&params, "sessionId") != self.session {
            return Ok(());
        }
        let turn = params["turnId"]
            .as_str()
            .or_else(|| params["item"]["turnId"].as_str())
            .map(str::to_owned);
        match method {
            "turn/started" => {
                let Some(turn) = turn else { return Ok(()) };
                if self.owns(&turn) {
                    return Ok(());
                }
                if self.active.is_some() {
                    self.joined.insert(turn);
                } else {
                    // A wake: Muse resumed on its own (background work ended).
                    self.begin(turn);
                    self.assistant = uuid::Uuid::new_v4().to_string();
                    self.started(
                        self.request
                            .model
                            .clone()
                            .unwrap_or_else(|| "default".into()),
                    )
                    .await?;
                }
            }
            "item/started" | "item/updated" | "item/completed" => {
                let item = &params["item"];
                let kind = string(item, "kind");
                let ours = turn.as_deref().is_none_or(|t| self.owns(t));
                if self.active.is_none() || !ours {
                    // Background work finishing outside a turn still counts down.
                    if matches!(kind, "subagent" | "workflow") {
                        self.norm.item(item);
                    }
                    return Ok(());
                }
                if kind == "userMessage" {
                    if self.seen_messages.insert(string(item, "itemId").to_owned()) {
                        if self.initial_message {
                            self.initial_message = false;
                        } else {
                            self.confirm_delivery().await?;
                        }
                    }
                    return Ok(());
                }
                for event in self.norm.item(item) {
                    self.emit(event).await?;
                }
            }
            "item/delta" => {
                if self.active.is_some() {
                    for event in self.norm.delta(&params) {
                        self.emit(event).await?;
                    }
                }
            }
            "turn/completed" => {
                let Some(turn) = turn else { return Ok(()) };
                if self.active.as_deref() == Some(&turn) {
                    let (status, error) = terminal(&params, self.interrupting);
                    self.error = error;
                    // Steers accepted into this turn were part of it.
                    while !self.deliveries.is_empty() && status == DoneStatus::Completed {
                        self.confirm_delivery().await?;
                    }
                    self.end(status).await?;
                } else {
                    self.joined.remove(&turn);
                }
            }
            "session/tokenUsage" if self.active.is_some() => {
                let usage = &params["usage"];
                self.emit(AgentEvent::Usage {
                    input_tokens: params["promptTokens"]
                        .as_u64()
                        .or_else(|| usage["inputTokens"].as_u64())
                        .unwrap_or(0),
                    cached_input_tokens: usage["cachedTokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["outputTokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: usage["reasoningTokens"].as_u64().unwrap_or(0),
                    context_limit: params["windowTokens"].as_u64(),
                })
                .await?;
            }
            "session/contextUsage" => {
                self.emit(AgentEvent::ContextWindow {
                    stats: komet_proto::ContextUsageStats::window(
                        params["usedTokens"].as_u64().unwrap_or(0),
                        params["windowTokens"].as_u64().unwrap_or(0),
                        komet_proto::ContextUsageSource::Native,
                        0,
                        0,
                        0,
                        0,
                    ),
                })
                .await?;
            }
            "approval/requested" => self.approve(&params)?,
            "userInput/requested" => self.ask(&params).await?,
            "userInput/settled" => {
                if let Some(task) = self.questions.remove(string(&params, "userInputId")) {
                    task.abort();
                }
            }
            "view/gap" => {
                self.emit(AgentEvent::Error {
                    message: "Muse dropped part of this session's event stream".into(),
                    kind: komet_proto::ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                })
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn end(&mut self, status: DoneStatus) -> Result<(), HarnessError> {
        self.active = None;
        self.first = false;
        self.joined.clear();
        self.interrupting = false;
        self.deliveries.clear();
        let error = self.error.take();
        self.emit(AgentEvent::Done {
            status,
            result: None,
            error,
            session_id: Some(self.session.clone()),
            reason: None,
        })
        .await?;
        self.norm.reset();
        Ok(())
    }

    /// Route Muse's approval through the engine permission bridge, then map
    /// the decision to one of the choices Muse actually offered.
    fn approve(&mut self, params: &Value) -> Result<(), HarnessError> {
        let choices = params["availableChoices"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (kind, summary) = approval_summary(params);
        let choice = if self.read_only {
            Some(komet_proto::PermissionChoice::Deny)
        } else if self.request.auto_approve {
            Some(komet_proto::PermissionChoice::Allow)
        } else if let Some(request) = self.permission.as_ref() {
            let receiver = request(
                kind,
                summary,
                vec![
                    komet_proto::PermissionChoice::Allow,
                    komet_proto::PermissionChoice::Deny,
                ],
            );
            let choices = choices.clone();
            let client = self.client.clone();
            let session = self.session.clone();
            let tx = self.tx.clone();
            let approval_id = params["approvalId"].clone();
            let requirement_id = params["currentRequirementId"].clone();
            self.dialogs.spawn(async move {
                let choice = receiver
                    .await
                    .unwrap_or(komet_proto::PermissionChoice::Deny);
                let decision = match choice {
                    komet_proto::PermissionChoice::Allow
                    | komet_proto::PermissionChoice::AllowAlways { .. } => "approved",
                    komet_proto::PermissionChoice::Deny => "abort",
                };
                let Some(native) = offered_choice(&choices, decision) else {
                    let _ = tx
                        .send(Err(HarnessError::Protocol(
                            "Muse offered no safe permission choice".into(),
                        )))
                        .await;
                    return;
                };
                if let Err(error) = client.request(
                    "approval/decide",
                    json!({
                        "sessionId": session,
                        "approvalId": approval_id,
                        "requirementId": requirement_id,
                        "choiceId": native["choiceId"],
                    }),
                ) {
                    let _ = tx.send(Err(error)).await;
                }
            });
            return Ok(());
        } else {
            Some(komet_proto::PermissionChoice::Deny)
        };
        let decision = match choice {
            Some(komet_proto::PermissionChoice::Allow)
            | Some(komet_proto::PermissionChoice::AllowAlways { .. }) => "approved",
            Some(komet_proto::PermissionChoice::Deny) | None => "abort",
        };
        let native = offered_choice(&choices, decision);
        let Some(native) = native else {
            return Err(HarnessError::Protocol(
                "Muse offered no safe permission choice".into(),
            ));
        };
        let id = self.client.request(
            "approval/decide",
            json!({
                "sessionId": self.session,
                "approvalId": params["approvalId"],
                "requirementId": params["currentRequirementId"],
                "choiceId": native["choiceId"],
            }),
        )?;
        self.pending.insert(id, Pending::Other("approval/decide"));
        Ok(())
    }

    /// A question for the user; answered (or cancelled) off the event loop.
    async fn ask(&mut self, params: &Value) -> Result<(), HarnessError> {
        let user_input = string(params, "userInputId").to_owned();
        let client = self.client.clone();
        let session = self.session.clone();
        let Some(input) = self.input.clone().filter(|_| !self.read_only) else {
            client.request(
                "userInput/cancel",
                json!({"sessionId": session, "userInputId": user_input,
                       "reason": "This run cannot ask the user."}),
            )?;
            return Ok(());
        };
        let raw: Vec<Value> = params["questions"].as_array().cloned().unwrap_or_default();
        let questions: Vec<UserInputQuestion> = raw
            .iter()
            .map(|q| UserInputQuestion {
                id: string(q, "id").into(),
                header: string(q, "header").into(),
                question: string(q, "question").into(),
                options: q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str().map(str::to_owned))
                    .collect(),
                multi_select: q["selection"]["mode"] == "multiple",
            })
            .collect();
        let answers = (input)(questions.clone());
        let id = user_input.clone();
        let tx = self.tx.clone();
        let task = self.dialogs.spawn(async move {
            let answers = answers.await.unwrap_or_default();
            let answers: Vec<Value> = questions
                .iter()
                .map(|question| {
                    let labels = answers
                        .iter()
                        .find(|a| a.question_id == question.id)
                        .map(|a| a.labels.clone())
                        .unwrap_or_default();
                    answer(question, &labels)
                })
                .collect();
            if let Err(error) = client.request(
                "userInput/answer",
                json!({"sessionId": session, "userInputId": id, "answers": answers}),
            ) {
                let _ = tx.send(Err(error)).await;
            }
        });
        self.questions.insert(user_input, task);
        Ok(())
    }
}

fn approval_summary(params: &Value) -> (komet_proto::PermissionKind, String) {
    let tool = string(params, "toolName");
    let args = params["rawArgs"]
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let command = args["command"].as_str().unwrap_or_default();
    let path = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or_default();
    let kind = match tool {
        "bash" | "shell" => komet_proto::PermissionKind::Command {
            cmdline: command.to_owned(),
        },
        "write" | "write_file" | "create_file" => komet_proto::PermissionKind::FileWrite {
            path: path.to_owned(),
        },
        _ => komet_proto::PermissionKind::Tool { name: tool.into() },
    };
    let details = if !command.is_empty() {
        command
    } else if !path.is_empty() {
        path
    } else {
        string(params, "rawArgs")
    };
    (kind, format!("Muse {tool}: {details}"))
}

fn offered_choice<'a>(choices: &'a [Value], decision: &str) -> Option<&'a Value> {
    choices
        .iter()
        .find(|choice| {
            string(choice, "decision") == decision
                && (decision != "approved" || string(choice, "scope") == "once")
        })
        .or_else(|| {
            (decision == "approved")
                .then(|| {
                    choices
                        .iter()
                        .find(|choice| string(choice, "decision") == "abort")
                })
                .flatten()
        })
}

/// One MSP answer: offered labels select, anything else is free text
/// (≤ 500 characters, the host's limit).
fn answer(question: &UserInputQuestion, labels: &[String]) -> Value {
    let (selected, free): (Vec<&String>, Vec<&String>) = labels
        .iter()
        .partition(|label| question.options.contains(label));
    let free_text: String = free
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(500)
        .collect();
    let mut answer = json!({"questionId": question.id});
    match (question.multi_select, selected.as_slice()) {
        (_, []) => {}
        (false, [first, ..]) => answer["selectedLabel"] = json!(first),
        (true, all) => answer["selectedLabels"] = json!(all),
    }
    if !free_text.is_empty() {
        answer[if selected.is_empty() {
            "freeText"
        } else {
            "note"
        }] = json!(free_text);
    } else if selected.is_empty() {
        // Every answer must say something; an unanswered question declines.
        answer["freeText"] = json!("(no answer)");
    }
    answer
}

/// Muse rejects a non-canonical workspace root (macOS `/tmp`), and on
/// Windows only the verbatim (`\\?\`) canonical form — which is what
/// `canonicalize` returns there.
fn workspace_root(cwd: &Path) -> String {
    std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Inline attachments as image parts, best-effort: the path refs already
/// ride the prompt text, so an unreadable or unsupported file must not fail
/// the turn.
async fn load_images(paths: &[String]) -> Vec<Value> {
    use base64::Engine as _;
    let mut parts = Vec::new();
    for path in paths {
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        if bytes.is_empty() || bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            continue;
        }
        let Some(media) = media_type(Path::new(path), &bytes) else {
            continue;
        };
        parts.push(json!({
            "type": "image",
            "base64Data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "mediaType": media,
        }));
    }
    parts
}

fn media_type(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    let by_extension = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_extension.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi: bool) -> UserInputQuestion {
        UserInputQuestion {
            id: "q".into(),
            header: "h".into(),
            question: "?".into(),
            options: vec!["Yes".into(), "No".into()],
            multi_select: multi,
        }
    }

    #[test]
    fn answers_select_offered_labels_and_carry_the_rest_as_text() {
        assert_eq!(
            answer(&question(false), &["Yes".into()]),
            json!({"questionId":"q","selectedLabel":"Yes"})
        );
        assert_eq!(
            answer(&question(true), &["Yes".into(), "No".into()]),
            json!({"questionId":"q","selectedLabels":["Yes","No"]})
        );
        assert_eq!(
            answer(&question(false), &["Maybe later".into()]),
            json!({"questionId":"q","freeText":"Maybe later"})
        );
        assert_eq!(
            answer(&question(false), &["No".into(), "because".into()]),
            json!({"questionId":"q","selectedLabel":"No","note":"because"})
        );
        assert_eq!(
            answer(&question(false), &[]),
            json!({"questionId":"q","freeText":"(no answer)"})
        );
    }

    #[test]
    fn images_are_typed_by_extension_or_magic() {
        assert_eq!(media_type(Path::new("a.PNG"), b""), Some("image/png"));
        assert_eq!(
            media_type(Path::new("paste"), &[0xFF, 0xD8, 0xFF, 0]),
            Some("image/jpeg")
        );
        assert_eq!(media_type(Path::new("a.svg"), b"<svg"), None);
    }
}
