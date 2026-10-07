//! Shared agent loop and node-scoped entry points for recursive work.
use crate::{
    AgentHandle, CancelOutcome, ChildSpec, ChildStatus, Effect, Limits, ModelRef, Owner,
    RecursionError, ToolCx, ToolOutput, ToolSelection, ToolsetFactory, TraceEvent, TraceSink,
    agent_tools, defaults,
    ledger::{Charge, Ledger, NodeId, Reservation, Settlement, estimate},
    messages::{
        self, Batch, Delivery, Envelope, Idle, Mailbox, MessageId, MessageKind, Refusal, Waited,
    },
    prompts,
    tool::{self, truncate},
};
use anyhow::{Result, bail};
use futures::{FutureExt, StreamExt};
use kyora_protocol::{
    ContentBlock, Message, ModelRequest, ModelResponse, RequestMeta, RequestOptions, Role,
    StopReason, StreamEvent, ToolResultPart, Usage,
};
use kyora_providers::{Accumulator, AttemptCharge, ModelProvider, ProviderError, RetryPolicy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// A final agent answer, text or a raw JSON value committed by a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Final visible text.
    Text(String),
    /// Structured final answer, preserving JSON bytes.
    Value(Box<RawValue>),
}
impl Answer {
    /// Text for a terminal or a leaf caller.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Value(v) => v.get().into(),
        }
    }
}
/// Node termination status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// An answer was returned.
    Completed,
    /// Turn cap reached.
    MaxTurns,
    /// No reservation fits.
    BudgetExhausted,
    /// Wall-clock deadline reached.
    Timeout,
    /// Input context is too large.
    ContextExhausted,
    /// Owner or user cancelled the node.
    Cancelled,
    /// Provider refused the request.
    Refused,
    /// Provider, toolset or runtime failed.
    Failed,
    /// A previously running node was recovered from a crash.
    Interrupted,
}
/// Result of an agent or traceable leaf node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
    /// Session-local node id.
    pub node: NodeId,
    /// Termination status.
    pub status: Status,
    /// Final or partial text answer.
    pub answer: Answer,
    /// Usage charged directly to this node.
    pub usage_self: Usage,
    /// Usage charged to this node and its descendants.
    pub usage_subtree: Usage,
    /// Admitted assistant turns.
    pub turns: u32,
}
/// Frozen identity passed to a toolset factory.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Node id.
    pub id: NodeId,
    /// Parent node.
    pub parent: Option<NodeId>,
    /// Agent depth.
    pub depth: u32,
    /// Model inherited by future child agents.
    pub model: ModelRef,
    /// Opaque initialization data supplied by the caller.
    pub init: Option<Arc<Value>>,
    /// Resolved tool selection for this node.
    pub selection: ToolSelection,
    /// Working directory.
    pub cwd: PathBuf,
}
/// Root task and immutable request prefix.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Initial user task.
    pub task: String,
    /// Root model reference.
    pub model: ModelRef,
    /// Workspace directory.
    pub cwd: PathBuf,
    /// Tool restriction.
    pub tools: ToolSelection,
    /// Optional system prompt override; otherwise the factual root prompt.
    pub system: Option<String>,
    /// Provider-neutral request settings.
    pub options: RequestOptions,
}
impl AgentSpec {
    /// Creates a task with default model, unrestricted tools and provider defaults.
    pub fn new(task: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            task: task.into(),
            model: defaults::DEFAULT_MODEL.parse().expect("default model"),
            cwd,
            tools: ToolSelection::default(),
            system: None,
            options: RequestOptions::default(),
        }
    }
}
/// Runtime dependencies and configurable defaults, all supplied by the frontend.
pub struct RuntimeConfig {
    /// Providers keyed by model-reference prefix.
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    /// Node toolset factory.
    pub toolsets: Arc<dyn ToolsetFactory>,
    /// Validated product limits.
    pub limits: Limits,
    /// Retry decisions for independent charged attempts.
    pub retry: RetryPolicy,
    /// Default leaf model.
    pub llm_model: ModelRef,
    /// Live and durable trace sink.
    pub trace: TraceSink,
    /// Session id (UUIDv7 even in no-session mode).
    pub session: String,
}
struct RuntimeInner {
    config: RuntimeConfig,
    ledger: Ledger,
    slots: Semaphore,
    cancel: CancellationToken,
    started: AtomicBool,
    panicked: AtomicBool,
    subagent_prompt: Mutex<String>,
    agents: Mutex<BTreeMap<NodeId, AgentEntry>>,
    messages: AtomicU64,
}
/// Directory entry used to address an agent and to wait for it.
#[derive(Clone)]
struct AgentEntry {
    parent: Option<NodeId>,
    name: String,
    state: Arc<NodeState>,
    cancel: CancellationToken,
    outcome: Option<watch::Receiver<Option<AgentOutcome>>>,
}
/// Shared runtime for one invocation. `run` may be called exactly once.
#[derive(Clone)]
pub struct Runtime(Arc<RuntimeInner>);
#[derive(Default)]
struct Work {
    closed: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Turns true once this node's latest delivery record is written.
    recorded: Option<watch::Receiver<bool>>,
    /// Turns true once this node's latest accepted send is queued or refused.
    sent: Option<watch::Receiver<bool>>,
}
#[derive(Default)]
struct NodeState {
    work: Mutex<Work>,
    tools: OnceLock<Vec<String>>,
    turns: AtomicU32,
    mailbox: Mailbox,
}
struct AgentSettings {
    name: String,
    origin_cell: Option<u32>,
    init: Option<Arc<Value>>,
    max_turns: u32,
    output: Option<Value>,
}
/// Node-scoped entry point for child agents and leaf completions.
#[derive(Clone)]
pub struct NodeCtx {
    /// Node id.
    pub id: NodeId,
    /// Parent node id.
    pub parent: Option<NodeId>,
    /// Agent depth.
    pub depth: u32,
    /// Node-owned cancellation token.
    pub cancel: CancellationToken,
    /// Deadline inherited by descendants.
    pub deadline: Instant,
    /// Parent model for future sub-agent inheritance.
    pub model: ModelRef,
    runtime: Runtime,
    state: Arc<NodeState>,
    cwd: PathBuf,
    options: RequestOptions,
    /// External owners on the path from the root: cell tokens of cell-owned agents
    /// and the caller token of a leaf. The node token always descends from the
    /// parent's, so node cancellation reaches the whole subtree synchronously.
    owners: Arc<Vec<CancellationToken>>,
}
/// One leaf completion without tools.
#[derive(Debug, Clone)]
pub struct LlmCall {
    /// Prompt text.
    pub prompt: String,
    /// Optional system prompt.
    pub system: Option<String>,
    /// Model override, otherwise the runtime's default leaf model.
    pub model: Option<ModelRef>,
    /// Output cap override, bounded by configured and discovered caps.
    pub max_tokens: Option<u32>,
    /// Provider-neutral options.
    pub options: RequestOptions,
}
impl LlmCall {
    /// Creates a leaf call with default settings.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            system: None,
            model: None,
            max_tokens: None,
            options: RequestOptions::default(),
        }
    }
}
/// Leaf completion result.
#[derive(Debug, Clone)]
pub struct LlmOutcome {
    /// Leaf node id.
    pub node: NodeId,
    /// Generated visible text.
    pub text: String,
    /// Model response including stop reason and usage.
    pub response: ModelResponse,
}
#[derive(Debug, thiserror::Error)]
enum CallError {
    #[error("budget exhausted")]
    Budget,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
impl Runtime {
    /// Validates configuration and creates a fresh run cancellation token and root ledger.
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        config.limits.validate()?;
        let ledger = Ledger::new(config.limits.clone())?;
        let slots = Semaphore::new(config.limits.max_inflight_requests as usize);
        Ok(Self(Arc::new(RuntimeInner {
            config,
            ledger,
            slots,
            cancel: CancellationToken::new(),
            started: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
            subagent_prompt: Mutex::new(prompts::SUBAGENT.into()),
            agents: Mutex::new(BTreeMap::new()),
            messages: AtomicU64::new(0),
        })))
    }
    /// Overrides the default child system prompt before the invocation starts.
    pub fn set_subagent_prompt(&self, prompt: String) -> Result<()> {
        let mut frozen = self
            .0
            .subagent_prompt
            .lock()
            .expect("prompt mutex poisoned");
        if self.0.started.load(Ordering::SeqCst) {
            bail!("runtime already invoked");
        }
        *frozen = prompt;
        Ok(())
    }
    /// Cancels this invocation and all node-owned requests.
    pub fn cancel(&self) {
        self.0.cancel.cancel();
    }
    /// Accesses the invocation cancellation token.
    pub fn cancellation(&self) -> CancellationToken {
        self.0.cancel.clone()
    }
    /// Returns shared accounting.
    pub fn ledger(&self) -> &Ledger {
        &self.0.ledger
    }
    /// Runs a root agent, closes its scope and writes session_end.
    pub async fn run(&self, spec: AgentSpec) -> Result<AgentOutcome> {
        {
            // Freeze configuration atomically with admission of the root.
            let _prompt = self
                .0
                .subagent_prompt
                .lock()
                .expect("prompt mutex poisoned");
            if self.0.started.swap(true, Ordering::SeqCst) {
                bail!("runtime already invoked");
            }
        }
        let _cancel_on_drop = CancelOnDrop(self.0.cancel.clone());
        let runtime = self.clone();
        tokio::spawn(async move { runtime.run_owned(spec).await }).await?
    }
    async fn run_owned(&self, spec: AgentSpec) -> Result<AgentOutcome> {
        let cx = NodeCtx {
            id: 0,
            parent: None,
            depth: 0,
            cancel: self.0.cancel.child_token(),
            deadline: Instant::now() + self.0.config.limits.run_timeout,
            model: spec.model.clone(),
            runtime: self.clone(),
            state: Arc::new(NodeState::default()),
            cwd: spec.cwd.clone(),
            options: spec.options.clone(),
            owners: Arc::new(Vec::new()),
        };
        self.register(
            0,
            None,
            "root".into(),
            cx.state.clone(),
            cx.cancel.clone(),
            None,
        );
        self.emit(TraceEvent::SessionStart {
            session: self.0.config.session.clone(),
            cwd: spec.cwd.clone(),
            kyora: env!("CARGO_PKG_VERSION").into(),
            limits: self.0.config.limits.clone(),
        })
        .await?;
        let result = self
            .run_node(
                cx,
                spec,
                AgentSettings {
                    name: "root".into(),
                    origin_cell: None,
                    init: None,
                    max_turns: self.0.config.limits.max_turns,
                    output: None,
                },
                None,
            )
            .await;
        self.emit(TraceEvent::SessionEnd {
            status: result.as_ref().map_or(Status::Failed, |o| o.status),
        })
        .await?;
        result
    }
    async fn run_node(
        &self,
        cx: NodeCtx,
        spec: AgentSpec,
        settings: AgentSettings,
        owner: Option<CancellationToken>,
    ) -> Result<AgentOutcome> {
        let token = cx.cancel.clone();
        let deadline = cx.deadline;
        let watcher = tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {},
                _ = tokio::time::sleep_until(deadline) => token.cancel(),
                _ = async { match owner { Some(owner) => owner.cancelled().await, None => std::future::pending().await } } => token.cancel(),
            }
        });
        let result = AssertUnwindSafe(self.agent(&cx, spec, settings))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                self.0.panicked.store(true, Ordering::SeqCst);
                Err(anyhow::anyhow!("agent task panicked"))
            });
        // Refuse new messages before children are cancelled, so their notices and any
        // late sends fail visibly instead of queueing for an agent that has ended.
        for message in cx.state.mailbox.close() {
            self.undelivered(message).await;
        }
        self.join_descendants(&cx).await;
        watcher.abort();
        let _ = watcher.await;
        let mut outcome = result.as_ref().cloned().unwrap_or_else(|_| {
            self.outcome(
                cx.id,
                Status::Failed,
                Answer::Text(String::new()),
                cx.state.turns.load(Ordering::SeqCst),
            )
        });
        if cx.id == 0 && self.0.panicked.load(Ordering::SeqCst) {
            outcome.status = Status::Failed;
        }
        (outcome.usage_self, outcome.usage_subtree) = self.0.ledger.usage(cx.id);
        let emitted = self
            .emit(TraceEvent::NodeEnd {
                outcome: outcome.clone(),
            })
            .await;
        self.0.ledger.shutdown(cx.id);
        emitted?;
        if cx.parent.is_some() {
            Ok(outcome)
        } else {
            result.map(|_| outcome)
        }
    }
    async fn join_descendants(&self, cx: &NodeCtx) {
        let tasks = {
            let mut work = cx.state.work.lock().expect("node work mutex poisoned");
            work.closed = true;
            self.0.ledger.close_admission(cx.id);
            // Every child and leaf token descends from the node token, so this stops
            // the whole subtree at once, including tasks that have not started yet.
            cx.cancel.cancel();
            std::mem::take(&mut work.tasks)
        };
        for task in tasks {
            if task.await.is_err() {
                self.0.panicked.store(true, Ordering::SeqCst);
            }
        }
    }
    async fn emit(&self, event: TraceEvent) -> Result<()> {
        self.0.config.trace.emit(event).await
    }
    fn outcome(&self, node: NodeId, status: Status, answer: Answer, turns: u32) -> AgentOutcome {
        let (usage_self, usage_subtree) = self.0.ledger.usage(node);
        AgentOutcome {
            node,
            status,
            answer,
            usage_self,
            usage_subtree,
            turns,
        }
    }
    async fn message(
        &self,
        node: NodeId,
        history: &mut Vec<Message>,
        message: Message,
    ) -> Result<()> {
        self.emit(TraceEvent::Message {
            node,
            message: message.clone(),
        })
        .await?;
        history.push(message);
        Ok(())
    }
    async fn agent(
        &self,
        cx: &NodeCtx,
        spec: AgentSpec,
        settings: AgentSettings,
    ) -> Result<AgentOutcome> {
        let info = NodeInfo {
            id: cx.id,
            parent: cx.parent,
            depth: cx.depth,
            model: cx.model.clone(),
            cwd: spec.cwd.clone(),
            init: settings.init,
            selection: spec.tools.clone(),
        };
        let tools = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.0
                .config
                .toolsets
                .toolset(&info, &spec.tools)
                .and_then(|tools| tools.select(&spec.tools))
        }))
        .unwrap_or_else(|_| {
            self.0.panicked.store(true, Ordering::SeqCst);
            Err(anyhow::anyhow!("toolset factory panicked"))
        })
        .and_then(|tools| match &settings.output {
            // A child with an output contract finishes through submit_result.
            Some(schema) => tools.with_own_validation(Arc::new(agent_tools::SubmitResult::new(
                schema.clone(),
                self.0.config.limits.message_chars,
            ))),
            None => Ok(tools),
        });
        let specs = tools
            .as_ref()
            .map_or_else(|_| Vec::new(), |tools| tools.specs());
        let system = spec.system.unwrap_or_else(|| prompts::root(&specs));
        let mut node_limits = self.0.config.limits.clone();
        node_limits.max_turns = settings.max_turns;
        node_limits.budget_tokens = cx.budget().limit;
        self.emit(TraceEvent::NodeStart {
            node: cx.id,
            parent: cx.parent,
            depth: cx.depth,
            kind: "agent".into(),
            name: settings.name,
            origin_cell: settings.origin_cell,
            model: spec.model.to_string(),
            system: Some(system.clone()),
            tools: specs.clone(),
            limits: node_limits,
            prompt: None,
        })
        .await?;
        let tools = tools?;
        cx.state
            .tools
            .set(specs.iter().map(|tool| tool.name.clone()).collect())
            .expect("toolset frozen once");
        let mut history = Vec::new();
        // Messages sent before the first request follow the task.
        self.user_turn(
            cx,
            &mut history,
            vec![ContentBlock::Text { text: spec.task }],
            true,
        )
        .await?;
        let mut turns = 0;
        let mut answer = Answer::Text(String::new());
        let mut previous = None;
        let mut output_cap = self.0.config.limits.max_output_tokens;
        let mut invalid_retry = false;
        let contract = settings.output.is_some();
        let mut reminded = false;
        let status = loop {
            if cx.stopped() || Instant::now() >= cx.deadline {
                break cx.cancel_status();
            }
            if turns >= settings.max_turns {
                break Status::MaxTurns;
            }
            // M1 compaction is deliberately a no-op at this round boundary.
            let req = ModelRequest {
                model: spec.model.model.clone(),
                system: Some(system.clone()),
                messages: history.clone(),
                tools: specs.clone(),
                max_tokens: output_cap,
                options: spec.options.clone(),
                metadata: RequestMeta {
                    node_id: Some(cx.id.to_string()),
                    depth: cx.depth,
                },
            };
            let sent = req.messages.len();
            let (mut response, mut invalid) =
                match self.attempts(cx, &spec.model, req, previous).await {
                    Ok(r) => r,
                    Err(e) => {
                        self.emit_error(cx.id, &e).await?;
                        break status_for(&e, cx);
                    }
                };
            for (id, name, input) in (Message {
                role: Role::Assistant,
                content: response.content.clone(),
            })
            .tool_uses()
            {
                if tools.get(name).is_some() && tools.validate(name, input).is_err() {
                    invalid
                        .entry(id.into())
                        .or_insert_with(|| input.to_string());
                }
            }
            for (call, raw) in &invalid {
                self.emit(TraceEvent::InvalidToolInput {
                    node: cx.id,
                    call: call.clone(),
                    raw: raw.clone(),
                })
                .await?;
            }
            if !invalid.is_empty()
                && response.stop_reason == StopReason::MaxTokens
                && !invalid_retry
            {
                invalid_retry = true;
                output_cap = output_cap.saturating_mul(2);
                self.emit(TraceEvent::StreamReset { node: cx.id }).await?;
                // History has not changed, so the old estimator still describes this request.
                continue;
            }
            invalid_retry = false;
            output_cap = self.0.config.limits.max_output_tokens;
            previous = Some((response.usage, sent));
            turns += 1;
            cx.state.turns.store(turns, Ordering::SeqCst);
            for block in &mut response.content {
                if let ContentBlock::ToolUse { id, input, .. } = block
                    && invalid.contains_key(id)
                {
                    *input = serde_json::json!({});
                }
            }
            let assistant = Message {
                role: Role::Assistant,
                content: response.content.clone(),
            };
            if !assistant.text().is_empty() {
                answer = Answer::Text(assistant.text());
            }
            self.message(cx.id, &mut history, assistant.clone()).await?;
            let mut results = Vec::new();
            let mut final_answer = None;
            // Non-executable tool calls are handled here before branching on stop reason.
            for (id, name, input) in assistant.tool_uses() {
                self.emit(TraceEvent::ToolCall {
                    node: cx.id,
                    call: id.into(),
                    name: name.into(),
                    input: invalid
                        .get(id)
                        .map_or_else(|| input.clone(), |raw| Value::String(raw.clone())),
                })
                .await?;
                let mut whole = false;
                let mut result = if let Some(raw) = invalid.get(id) {
                    ToolOutput::error(serde_json::json!({"INVALID_JSON": raw}).to_string())
                } else if response.stop_reason != StopReason::ToolUse {
                    ToolOutput::error(format!(
                        "not executed: {}",
                        serde_json::to_value(&response.stop_reason)?
                            .as_str()
                            .unwrap_or("unknown")
                    ))
                } else if final_answer.is_some() {
                    ToolOutput::error("skipped: final answer already committed")
                } else if cx.stopped() || Instant::now() >= cx.deadline {
                    ToolOutput::error("cancelled")
                } else if let Err(error) = tools.validate(name, input) {
                    ToolOutput::error(error.to_string())
                } else {
                    let tool = tools.get(name).expect("validated tool");
                    whole = !tool.truncated();
                    let tool_cx = ToolCx {
                        node: cx.clone(),
                        call_id: id.into(),
                        cwd: spec.cwd.clone(),
                        cancel: cx.cancel.child_token(),
                        events: self.0.config.trace.clone(),
                    };
                    if tool.effect() == Effect::Mutating {
                        // A started mutation must finish before its result and session_end.
                        // The run watcher delivers cancellation through tool_cx.
                        tool.call(input.clone(), tool_cx).await
                    } else {
                        tokio::select! {
                            biased;
                            _ = cx.cancel.cancelled() => ToolOutput::error("cancelled"),
                            _ = tokio::time::sleep_until(cx.deadline) => { cx.cancel.cancel(); ToolOutput::error("cancelled") },
                            result = tool.call(input.clone(), tool_cx) => result,
                        }
                    }
                };
                // An answer that breaks the output contract does not finish the node.
                if final_answer.is_none()
                    && let Some(committed) = result.final_answer.take()
                {
                    match self.accept(settings.output.as_ref(), committed) {
                        Ok(committed) => final_answer = Some(committed),
                        Err(reason) => {
                            result.is_error = true;
                            result.content.push(ToolResultPart::Text {
                                text: format!("\nfinal answer not accepted: {reason}"),
                            });
                        }
                    }
                }
                let content = match result.text_content() {
                    text if whole => text,
                    text => truncate(&text, self.0.config.limits.tool_output_chars),
                };
                result.content = vec![ToolResultPart::Text {
                    text: content.clone(),
                }];
                self.emit(TraceEvent::ToolResult {
                    node: cx.id,
                    call: id.into(),
                    content,
                    is_error: result.is_error,
                })
                .await?;
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: result.content,
                    is_error: result.is_error,
                });
            }
            let has_tools = !results.is_empty();
            if has_tools {
                // Pending messages follow all tool results, never sit between them, and
                // are only taken when another request will carry them.
                let continues = final_answer.is_none()
                    && !cx.stopped()
                    && Instant::now() < cx.deadline
                    && turns < settings.max_turns
                    && matches!(
                        response.stop_reason,
                        StopReason::ToolUse | StopReason::PauseTurn | StopReason::MaxTokens
                    );
                self.user_turn(cx, &mut history, results, continues).await?;
            }
            // An accepted final answer is the terminal decision. A cancellation that
            // arrives while its result is being recorded does not replace it.
            if let Some(value) = final_answer {
                answer = value;
                break Status::Completed;
            }
            if cx.stopped() || Instant::now() >= cx.deadline {
                break cx.cancel_status();
            }
            match response.stop_reason {
                StopReason::ToolUse | StopReason::PauseTurn => {}
                StopReason::EndTurn => {
                    answer = Answer::Text(assistant.text());
                    if turns >= settings.max_turns {
                        // A contract child that runs out of turns has not delivered.
                        break if contract {
                            Status::MaxTurns
                        } else {
                            Status::Completed
                        };
                    }
                    // With children still running or messages pending, wait for them
                    // instead of ending; the next turn starts with whatever arrived.
                    // A contract child keeps its mailbox open, since it may go on.
                    match self.idle(cx, !contract).await {
                        Some(batch) => self.deliver(cx, &mut history, Vec::new(), batch).await?,
                        None if cx.stopped() || Instant::now() >= cx.deadline => {
                            break cx.cancel_status();
                        }
                        None if !contract => break Status::Completed,
                        None if !reminded => {
                            reminded = true;
                            self.user_turn(
                                cx,
                                &mut history,
                                vec![ContentBlock::Text {
                                    text: agent_tools::SUBMIT_REMINDER.into(),
                                }],
                                true,
                            )
                            .await?;
                        }
                        None => {
                            let message = "ended without calling submit_result";
                            self.emit(TraceEvent::Error {
                                node: cx.id,
                                message: message.into(),
                            })
                            .await?;
                            answer = Answer::Text(match assistant.text() {
                                text if text.is_empty() => format!("{message}."),
                                text => format!("{message}. Last reply: {text}"),
                            });
                            break Status::Failed;
                        }
                    }
                }
                StopReason::MaxTokens => {
                    if !has_tools {
                        self.user_turn(
                            cx,
                            &mut history,
                            vec![ContentBlock::Text {
                                text: "continue".into(),
                            }],
                            turns < settings.max_turns,
                        )
                        .await?;
                    }
                }
                StopReason::ModelContextWindowExceeded => break Status::ContextExhausted,
                StopReason::Refusal => break Status::Refused,
                _ => break Status::Failed,
            }
        };
        Ok(self.outcome(cx.id, status, answer, turns))
    }
    /// Checks a committed final answer against the node's output contract: a node
    /// with an output schema finishes only with a JSON value that the schema accepts
    /// and that fits one message body.
    fn accept(
        &self,
        output: Option<&Value>,
        answer: Answer,
    ) -> std::result::Result<Answer, String> {
        let Some(schema) = output else {
            return Ok(answer);
        };
        let Answer::Value(raw) = &answer else {
            return Err("a task with an output schema finishes only through submit_result".into());
        };
        let value: Value = serde_json::from_str(raw.get())
            .map_err(|error| format!("result is not JSON: {error}"))?;
        tool::validate(schema, &value)
            .map_err(|error| format!("result does not match the output schema: {error}"))?;
        let cap = self.0.config.limits.message_chars;
        if raw.get().chars().count() > cap {
            return Err(format!("result exceeds {cap} characters"));
        }
        Ok(answer)
    }
    /// Records a user message, appending pending messages when `deliver` is set.
    async fn user_turn(
        &self,
        cx: &NodeCtx,
        history: &mut Vec<Message>,
        content: Vec<ContentBlock>,
        deliver: bool,
    ) -> Result<()> {
        if deliver {
            let batch = cx.state.mailbox.take(
                self.0.config.limits.delivery_chars,
                |_| true,
                |message| self.size(message),
            );
            if !batch.taken.is_empty() {
                return self.deliver(cx, history, content, batch).await;
            }
        }
        self.message(
            cx.id,
            history,
            Message {
                role: Role::User,
                content,
            },
        )
        .await
    }
    /// Appends one delivery as text blocks after `content`, says how many messages
    /// still wait, and records the user message.
    async fn deliver(
        &self,
        cx: &NodeCtx,
        history: &mut Vec<Message>,
        mut content: Vec<ContentBlock>,
        batch: Batch,
    ) -> Result<()> {
        let mut written = {
            let mut work = cx.state.work.lock().expect("node work mutex poisoned");
            cx.record_delivery(&mut work, &batch.taken, Delivery::Turn)
        };
        // The delivery record precedes the conversation record that carries the text.
        while !*written.borrow_and_update() {
            if written.changed().await.is_err() {
                break;
            }
        }
        content.extend(batch.taken.iter().map(|message| ContentBlock::Text {
            text: self.render(message),
        }));
        if batch.queued > 0 {
            content.push(ContentBlock::Text {
                text: format!("[{}]", messages::more(batch.queued)),
            });
        }
        self.message(
            cx.id,
            history,
            Message {
                role: Role::User,
                content,
            },
        )
        .await
    }
    /// Waits at the end of a turn until messages arrive or nothing more can arrive.
    /// Returns None once nothing more can arrive or the node is cancelled.
    async fn idle(&self, cx: &NodeCtx, close: bool) -> Option<Batch> {
        loop {
            let mut changed = cx.state.mailbox.subscribe();
            let budget = self.0.config.limits.delivery_chars;
            match cx
                .state
                .mailbox
                .idle(close, budget, |message| self.size(message))
            {
                Idle::Deliver(batch) => return Some(batch),
                Idle::Done => return None,
                Idle::Wait => {}
            }
            tokio::select! {
                biased;
                _ = cx.cancel.cancelled() => return None,
                _ = tokio::time::sleep_until(cx.deadline) => return None,
                _ = changed.changed() => {}
            }
        }
    }
    fn register(
        &self,
        id: NodeId,
        parent: Option<NodeId>,
        name: String,
        state: Arc<NodeState>,
        cancel: CancellationToken,
        outcome: Option<watch::Receiver<Option<AgentOutcome>>>,
    ) {
        self.agents().insert(
            id,
            AgentEntry {
                parent,
                name,
                state,
                cancel,
                outcome,
            },
        );
    }
    fn agents(&self) -> std::sync::MutexGuard<'_, BTreeMap<NodeId, AgentEntry>> {
        self.0
            .agents
            .lock()
            .expect("agent directory mutex poisoned")
    }
    fn name(&self, id: NodeId) -> String {
        self.agents()
            .get(&id)
            .map(|entry| entry.name.clone())
            .unwrap_or_default()
    }
    fn render(&self, message: &Envelope) -> String {
        message.render(&self.name(message.from))
    }
    /// Size of a message as delivered, counted against `Limits::delivery_chars`.
    fn size(&self, message: &Envelope) -> usize {
        self.render(message).chars().count()
    }
    fn next_message(&self) -> MessageId {
        self.0.messages.fetch_add(1, Ordering::SeqCst)
    }
    // A failed trace write also fails node_end and so surfaces there; delivery
    // bookkeeping does not depend on message records.
    async fn undelivered(&self, message: Envelope) {
        let _ = self
            .emit(TraceEvent::MessageUndelivered {
                message,
                reason: "recipient ended".into(),
            })
            .await;
    }
    /// Posts a node-owned child's terminal notice to its parent's mailbox.
    async fn notify(&self, parent: NodeId, mailbox: &Mailbox, outcome: &AgentOutcome) {
        let message = Envelope {
            id: self.next_message(),
            from: outcome.node,
            to: parent,
            kind: MessageKind::for_status(outcome.status),
            body: truncate(&outcome.answer.text(), self.0.config.limits.message_chars),
            sent_at: chrono::Utc::now(),
            spawn: Some(outcome.node),
            status: Some(outcome.status),
        };
        if !mailbox.is_closed() {
            let _ = self
                .emit(TraceEvent::MessageSent {
                    message: message.clone(),
                })
                .await;
        }
        if let Err(message) = mailbox.push(message) {
            self.undelivered(message).await;
        }
    }
    async fn emit_error(&self, node: NodeId, error: &CallError) -> Result<()> {
        self.emit(TraceEvent::Error {
            node,
            message: error.to_string(),
        })
        .await
    }
    async fn attempts(
        &self,
        cx: &NodeCtx,
        model: &ModelRef,
        mut request: ModelRequest,
        previous: Option<(Usage, usize)>,
    ) -> std::result::Result<(ModelResponse, BTreeMap<String, String>), CallError> {
        let provider = self
            .0
            .config
            .providers
            .get(&model.provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider: {}", model.provider))?;
        if cx.stopped() || Instant::now() >= cx.deadline {
            return Err(ProviderError::Cancelled.into());
        }
        let model_info = tokio::select! {
            biased;
            _ = cx.cancel.cancelled() => return Err(ProviderError::Cancelled.into()),
            _ = tokio::time::sleep_until(cx.deadline) => return Err(ProviderError::Cancelled.into()),
            result = tokio::time::timeout(self.0.config.limits.request_total, provider.model_info(&model.model)) => result.map_err(|_| ProviderError::IdleTimeout)??,
        };
        request.max_tokens = request.max_tokens.min(
            model_info
                .max_output_tokens
                .unwrap_or(self.0.config.limits.max_output_tokens),
        );
        let prompt = estimate(&request, previous);
        let mut failures = 0;
        let mut unhinted = 0;
        loop {
            let permit = tokio::select! {
                biased;
                _ = cx.cancel.cancelled() => return Err(ProviderError::Cancelled.into()),
                _ = tokio::time::sleep_until(cx.deadline) => return Err(ProviderError::Cancelled.into()),
                permit = self.0.slots.acquire() => permit.map_err(|e| anyhow::anyhow!(e))?,
            };
            if cx.stopped() || Instant::now() >= cx.deadline {
                return Err(ProviderError::Cancelled.into());
            }
            let reservation = self
                .0
                .ledger
                .reserve(cx.id, prompt, request.max_tokens)
                .map_err(|_| CallError::Budget)?;
            let mut reservation = ReservationGuard::new(&self.0.ledger, reservation);
            let attempt = reservation.get().id;
            let max_tokens = reservation.get().max_tokens;
            if let Err(error) = self
                .emit(TraceEvent::AttemptStart {
                    node: cx.id,
                    attempt,
                    model: model.to_string(),
                    reserved: reservation.get().tokens,
                    max_tokens,
                })
                .await
            {
                reservation.settle(Charge::Failed(AttemptCharge::Zero));
                return Err(error.into());
            }
            request.max_tokens = max_tokens;
            let start = Instant::now();
            let mut partial = false;
            let dispatched = AssertUnwindSafe(async {
                if cx.stopped() || Instant::now() >= cx.deadline {
                    Err(ProviderError::NotSent("cancelled before dispatch".into()))
                } else {
                    reservation.dispatched = true;
                    let attempt_cancel = cx.cancel.child_token();
                    let mut attempt_cx = cx.clone();
                    attempt_cx.cancel = attempt_cancel.clone();
                    let future = self.stream(
                        &attempt_cx,
                        provider.as_ref(),
                        request.clone(),
                        &mut partial,
                    );
                    tokio::pin!(future);
                    tokio::select! {
                        biased;
                        result = &mut future => result,
                        _ = async {
                            tokio::select! {
                                _ = cx.cancel.cancelled() => {},
                                _ = tokio::time::sleep_until(cx.deadline) => {},
                                _ = tokio::time::sleep(self.0.config.limits.request_total) => {},
                            }
                        } => {
                            // Keep polling the same future so the provider can classify an
                            // unsent request. An unresponsive provider costs the reservation.
                            attempt_cancel.cancel();
                            let result = tokio::time::timeout(defaults::PROVIDER_CANCEL_GRACE, &mut future)
                                .await
                                .unwrap_or(Err(if cx.stopped() || Instant::now() >= cx.deadline {
                                    ProviderError::Cancelled
                                } else {
                                    ProviderError::IdleTimeout
                                }));
                            if matches!(result, Err(ProviderError::Cancelled))
                                && !cx.stopped() && Instant::now() < cx.deadline
                            {
                                Err(ProviderError::IdleTimeout)
                            } else {
                                result
                            }
                        },
                    }
                }
            })
            .catch_unwind()
            .await;
            let (result, panic) = match dispatched {
                Ok(result) => (result, None),
                Err(panic) => (
                    Err(ProviderError::Other("provider task panicked".into())),
                    Some(panic),
                ),
            };
            let charge = match &result {
                Ok((resp, _)) => Charge::Usage(resp.usage),
                Err(e) => Charge::Failed(e.charge()),
            };
            let settlement = reservation.settle(charge);
            drop(permit);
            self.emit(TraceEvent::AttemptEnd {
                node: cx.id,
                attempt,
                request_id: result.as_ref().ok().and_then(|(r, _)| r.id.clone()),
                outcome: result
                    .as_ref()
                    .map_or_else(|e| e.to_string(), |_| "completed".into()),
                charged: settlement.charged,
                excess: settlement.excess,
                usage: result.as_ref().ok().map(|(r, _)| r.usage),
                stop_reason: result.as_ref().ok().map(|(r, _)| r.stop_reason.clone()),
                ms: start.elapsed().as_millis() as u64,
            })
            .await?;
            if let Some(panic) = panic {
                std::panic::resume_unwind(panic);
            }
            match result {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if cx.stopped() || Instant::now() >= cx.deadline {
                        return Err(ProviderError::Cancelled.into());
                    }
                    failures += 1;
                    if matches!(
                        error,
                        ProviderError::Http {
                            status: 429,
                            retry_after: None,
                            ..
                        }
                    ) {
                        unhinted += 1;
                    }
                    // UUID randomness supplies independent full-jitter fractions without an RNG dependency.
                    let random = f64::from(u32::from_le_bytes(
                        uuid::Uuid::now_v7().as_bytes()[12..16]
                            .try_into()
                            .expect("uuid bytes"),
                    )) / (f64::from(u32::MAX) + 1.0);
                    let Some(delay) = self
                        .0
                        .config
                        .retry
                        .next_delay(&error, failures, unhinted, random)
                    else {
                        return Err(error.into());
                    };
                    if partial {
                        self.emit(TraceEvent::StreamReset { node: cx.id }).await?;
                    }
                    tokio::select! {
                        biased;
                        _ = cx.cancel.cancelled() => return Err(ProviderError::Cancelled.into()),
                        _ = tokio::time::sleep_until(cx.deadline) => return Err(ProviderError::Cancelled.into()),
                        _ = tokio::time::sleep(delay) => {},
                    }
                }
            }
        }
    }
    async fn stream(
        &self,
        cx: &NodeCtx,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        partial: &mut bool,
    ) -> std::result::Result<(ModelResponse, BTreeMap<String, String>), ProviderError> {
        let mut stream = tokio::time::timeout(
            self.0.config.limits.request_idle,
            provider.stream(request, cx.cancel.clone()),
        )
        .await
        .map_err(|_| ProviderError::IdleTimeout)??;
        let mut accumulator = Accumulator::new();
        let mut raw: BTreeMap<usize, (String, String)> = BTreeMap::new();
        while let Some(event) =
            tokio::time::timeout(self.0.config.limits.request_idle, stream.next())
                .await
                .map_err(|_| ProviderError::IdleTimeout)?
        {
            let event = event?;
            match &event {
                StreamEvent::BlockStart {
                    index,
                    block: kyora_protocol::BlockStart::ToolUse { id, .. },
                } => {
                    raw.insert(*index, (id.clone(), String::new()));
                }
                StreamEvent::ToolInputDelta {
                    index,
                    partial_json,
                } => {
                    if let Some((_, text)) = raw.get_mut(index) {
                        text.push_str(partial_json);
                    }
                }
                _ => {}
            }
            if matches!(
                event,
                StreamEvent::TextDelta { .. }
                    | StreamEvent::SignatureDelta { .. }
                    | StreamEvent::ThinkingDelta { .. }
                    | StreamEvent::ToolInputDelta { .. }
            ) {
                *partial = true;
                self.emit(TraceEvent::Delta {
                    node: cx.id,
                    event: event.clone(),
                })
                .await
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            }
            accumulator.push(event)?;
        }
        let invalid = accumulator
            .invalid_tool_inputs()
            .iter()
            .filter_map(|index| raw.get(index).cloned())
            .collect();
        Ok((accumulator.finish()?, invalid))
    }
}
impl NodeCtx {
    /// Whether this node must stop. External owners are checked directly, because
    /// the watchers that forward their cancellation to the node token may not have
    /// run yet; a cancelled owner cancels the node token here.
    fn stopped(&self) -> bool {
        if !self.cancel.is_cancelled() && self.owners.iter().any(CancellationToken::is_cancelled) {
            self.cancel.cancel();
        }
        self.cancel.is_cancelled()
    }
    fn check_open(&self, work: &Work) -> std::result::Result<(), RecursionError> {
        if work.closed || self.stopped() || Instant::now() >= self.deadline {
            return Err(RecursionError::Cancelled);
        }
        Ok(())
    }
    fn validate_model(&self, model: &ModelRef) -> std::result::Result<(), RecursionError> {
        if model.model.is_empty()
            || !self
                .runtime
                .0
                .config
                .providers
                .contains_key(&model.provider)
        {
            return Err(RecursionError::InvalidRequest(format!(
                "unknown model reference: {model}"
            )));
        }
        Ok(())
    }
    pub(crate) fn child_status(&self, outcome: Option<&AgentOutcome>) -> ChildStatus {
        let (usage_self, usage_subtree) = self.runtime.0.ledger.usage(self.id);
        ChildStatus {
            status: outcome.map(|o| o.status),
            turns: self.state.turns.load(Ordering::SeqCst),
            usage_self,
            usage_subtree,
        }
    }
    /// Atomically admits a child without waiting for slots or budget.
    /// Explicit tools must be a subset of this node's frozen tool names.
    pub fn spawn_agent(
        &self,
        spec: ChildSpec,
        owner: Owner,
    ) -> std::result::Result<AgentHandle, RecursionError> {
        let mut work = self.state.work.lock().expect("node work mutex poisoned");
        self.check_open(&work)?;
        if spec.max_turns == Some(0) || spec.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(RecursionError::InvalidRequest(
                "turns and timeout must be positive".into(),
            ));
        }
        let deadline = match spec.timeout {
            Some(timeout) => Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| RecursionError::InvalidRequest("timeout is too large".into()))?
                .min(self.deadline),
            None => self.deadline,
        };
        let model = spec.model.unwrap_or_else(|| self.model.clone());
        self.validate_model(&model)?;
        let names = self
            .state
            .tools
            .get()
            .ok_or_else(|| RecursionError::InvalidRequest("toolset not frozen".into()))?;
        let selection = ToolSelection(Some(match spec.tools.0 {
            Some(requested) => {
                if requested
                    .iter()
                    .any(|name| name == agent_tools::SUBMIT_RESULT)
                {
                    return Err(RecursionError::InvalidRequest(
                        "submit_result comes with an output schema".into(),
                    ));
                }
                if let Some(name) = requested.iter().find(|name| !names.contains(name)) {
                    return Err(RecursionError::InvalidRequest(format!(
                        "tool not held by parent: {name}"
                    )));
                }
                requested
            }
            None => defaults::SUBAGENT_TOOLS
                .iter()
                .filter(|name| names.iter().any(|held| held == **name))
                .map(|name| (*name).into())
                .collect(),
        }));
        if let Some(output) = &spec.output {
            if output["type"] != "object" {
                return Err(RecursionError::InvalidRequest(
                    "output must be a JSON schema of type object".into(),
                ));
            }
            tool::check_schema(output).map_err(|error| {
                RecursionError::InvalidRequest(format!("invalid output schema: {error}"))
            })?;
        }
        let owner_token = match owner {
            Owner::Node => None,
            Owner::Cell(token) => {
                if token.is_cancelled() {
                    return Err(RecursionError::Cancelled);
                }
                Some(token)
            }
        };
        // The child token descends from this node's token even when a cell owns the
        // child; the cell token is an additional owner, checked directly.
        let cancel = self.cancel.child_token();
        let mut owners = self.owners.to_vec();
        owners.extend(owner_token.clone());
        let id = self.runtime.0.ledger.admit(self.id, true, spec.budget)?;
        let cx = NodeCtx {
            id,
            parent: Some(self.id),
            depth: self.depth + 1,
            cancel,
            deadline,
            model: model.clone(),
            runtime: self.runtime.clone(),
            state: Arc::new(NodeState::default()),
            cwd: self.cwd.clone(),
            options: self.options.clone(),
            owners: Arc::new(owners),
        };
        let task = match spec.preamble {
            Some(preamble) => format!("{}\n\n{preamble}", spec.task),
            None => spec.task,
        };
        let agent_spec = AgentSpec {
            task,
            model,
            cwd: self.cwd.clone(),
            tools: selection,
            system: Some(
                self.runtime
                    .0
                    .subagent_prompt
                    .lock()
                    .expect("prompt mutex poisoned")
                    .clone(),
            ),
            options: self.options.clone(),
        };
        let settings = AgentSettings {
            name: spec.name.unwrap_or_default(),
            origin_cell: spec.origin_cell,
            init: spec.init,
            max_turns: spec
                .max_turns
                .unwrap_or(self.runtime.0.config.limits.subagent_max_turns),
            output: spec.output,
        };
        let (tx, outcome) = tokio::sync::watch::channel(None);
        let handle = AgentHandle {
            id,
            node: Arc::new(cx.clone()),
            outcome,
        };
        // A node-owned child reports its ending to this agent's mailbox. The result of a
        // cell-owned child belongs to the cell that owns it, so no notice is posted.
        let notify = owner_token.is_none();
        self.runtime.register(
            id,
            Some(self.id),
            settings.name.clone(),
            cx.state.clone(),
            cx.cancel.clone(),
            Some(handle.outcome.clone()),
        );
        if notify {
            self.state.mailbox.expect(id);
        }
        let parent = self.id;
        let parent_state = self.state.clone();
        let runtime = self.runtime.clone();
        work.tasks.push(tokio::spawn(async move {
            let state = cx.state.clone();
            let result = runtime
                .run_node(cx, agent_spec, settings, owner_token)
                .await;
            let result = result.unwrap_or_else(|_| {
                runtime.outcome(
                    id,
                    Status::Failed,
                    Answer::Text(String::new()),
                    state.turns.load(Ordering::SeqCst),
                )
            });
            tx.send_replace(Some(result.clone()));
            // The handle resolves first, so a parent woken by the notice can read the outcome.
            if notify {
                runtime.notify(parent, &parent_state.mailbox, &result).await;
            } else {
                parent_state.mailbox.touch();
            }
        }));
        Ok(handle)
    }
    /// Resolves an address relative to this agent: "parent", a node id such as "3"
    /// or "#3", or the name of its parent, a child or a sibling. Ids win over names.
    pub fn resolve(&self, address: &str) -> std::result::Result<NodeId, RecursionError> {
        let address = address.trim();
        let agents = self.runtime.agents();
        let parent = agents.get(&self.id).and_then(|entry| entry.parent);
        if address == "parent" {
            return parent.ok_or_else(|| {
                RecursionError::InvalidRequest("the root agent has no parent".into())
            });
        }
        if let Ok(id) = address.strip_prefix('#').unwrap_or(address).parse() {
            return Ok(id);
        }
        let named = agents
            .iter()
            .filter(|(id, entry)| {
                **id != self.id
                    && !entry.name.is_empty()
                    && entry.name == address
                    && (Some(**id) == parent
                        || entry.parent == Some(self.id)
                        || (parent.is_some() && entry.parent == parent))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        match named[..] {
            [id] => Ok(id),
            [] => Err(RecursionError::InvalidRequest(format!(
                "unknown agent: {address}"
            ))),
            _ => Err(RecursionError::InvalidRequest(format!(
                "ambiguous agent name: {address}; use its id"
            ))),
        }
    }
    /// Returns the recipient's state and the spawn a message to it belongs to.
    /// Addressing is limited to this agent's parent, children and siblings.
    fn kin(
        &self,
        to: NodeId,
    ) -> std::result::Result<(Arc<NodeState>, Option<NodeId>), RecursionError> {
        let agents = self.runtime.agents();
        let unknown = || RecursionError::InvalidRequest(format!("unknown agent: {to}"));
        let me = agents.get(&self.id).ok_or_else(unknown)?;
        let target = agents.get(&to).ok_or_else(unknown)?;
        if to == self.id {
            return Err(RecursionError::InvalidRequest(
                "cannot send a message to yourself".into(),
            ));
        }
        let spawn = if me.parent == Some(to) {
            Some(self.id)
        } else if target.parent == Some(self.id) {
            Some(to)
        } else if me.parent.is_some() && me.parent == target.parent {
            None
        } else {
            return Err(RecursionError::InvalidRequest(format!(
                "agent {to} is not this agent's parent, child or sibling"
            )));
        };
        Ok((target.state.clone(), spawn))
    }
    /// Queues a message for the parent, a child or a sibling and returns its id
    /// without waiting for the recipient. It is delivered at the recipient's next
    /// turn boundary or by its receive call. A full mailbox refuses it at once.
    pub async fn send(
        &self,
        to: NodeId,
        body: impl Into<String>,
    ) -> std::result::Result<MessageId, RecursionError> {
        let limits = &self.runtime.0.config.limits;
        let body = body.into();
        if body.chars().count() > limits.message_chars {
            return Err(RecursionError::InvalidRequest(format!(
                "message exceeds {} characters",
                limits.message_chars
            )));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut work = self.state.work.lock().expect("node work mutex poisoned");
            self.check_open(&work)?;
            let (recipient, spawn) = self.kin(to)?;
            recipient
                .mailbox
                .reserve(limits.mailbox_capacity as usize)
                .map_err(|refusal| match refusal {
                    Refusal::Full => RecursionError::MailboxFull { agent: to },
                    Refusal::Closed => RecursionError::AgentFinished { agent: to },
                })?;
            let message = Envelope {
                id: self.runtime.next_message(),
                from: self.id,
                to,
                kind: MessageKind::Message,
                body,
                sent_at: chrono::Utc::now(),
                spawn,
                status: None,
            };
            let runtime = self.runtime.clone();
            // Each send goes after this sender's previous one, whatever order their
            // records are acknowledged in, so a sender's messages keep their order.
            let (done, queued) = watch::channel(false);
            let mut previous = work.sent.replace(queued);
            // Recording and queueing run as owned work that this node's shutdown joins,
            // so a caller that stops waiting cannot leave an accepted send half done.
            work.tasks.push(tokio::spawn(async move {
                if let Some(previous) = &mut previous {
                    while !*previous.borrow_and_update() {
                        if previous.changed().await.is_err() {
                            break;
                        }
                    }
                }
                let id = message.id;
                // The send record precedes any delivery record.
                let _ = runtime
                    .emit(TraceEvent::MessageSent {
                        message: message.clone(),
                    })
                    .await;
                let result = match recipient.mailbox.push(message) {
                    Ok(()) => Ok(id),
                    Err(message) => {
                        runtime.undelivered(message).await;
                        Err(RecursionError::AgentFinished { agent: to })
                    }
                };
                done.send_replace(true);
                let _ = tx.send(result);
            }));
        }
        rx.await.unwrap_or(Err(RecursionError::Cancelled))
    }
    /// Takes one delivery of pending messages: whole messages in arrival order within
    /// `Limits::delivery_chars`, at least one. When none is pending, waits up to
    /// `yield_after` for the first one, and returns an empty list if none arrives.
    pub async fn receive(
        &self,
        yield_after: Duration,
    ) -> std::result::Result<Vec<Envelope>, RecursionError> {
        let until = Instant::now()
            .checked_add(yield_after)
            .map_or(self.deadline, |until| until.min(self.deadline));
        loop {
            let mut changed = self.state.mailbox.subscribe();
            {
                let mut work = self.state.work.lock().expect("node work mutex poisoned");
                self.check_open(&work)?;
                // Taking and handing over are one step without an await, so a caller
                // that stops waiting cannot lose messages; owned work records them.
                let batch = self.state.mailbox.take(
                    self.runtime.0.config.limits.delivery_chars,
                    |_| true,
                    |message| self.runtime.size(message),
                );
                if !batch.taken.is_empty() {
                    self.record_delivery(&mut work, &batch.taken, Delivery::Receive);
                    return Ok(batch.taken);
                }
                if self.state.mailbox.is_closed() {
                    return Err(RecursionError::Cancelled);
                }
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(RecursionError::Cancelled),
                _ = tokio::time::sleep_until(until) => return Ok(Vec::new()),
                _ = changed.changed() => {}
            }
        }
    }
    /// Waits until the given children have finished; with None, every child whose
    /// result has not been delivered yet. Returns early, listing the children still
    /// running, when `timeout` passes. Takes what the finished children had queued,
    /// their unread messages and notices in arrival order, so none of it is delivered
    /// again at the next turn and each child's order is kept.
    pub async fn wait(
        &self,
        agents: Option<&[NodeId]>,
        timeout: Option<Duration>,
    ) -> std::result::Result<Waited, RecursionError> {
        self.wait_via(agents, timeout, Delivery::Wait).await
    }
    async fn wait_via(
        &self,
        agents: Option<&[NodeId]>,
        timeout: Option<Duration>,
        via: Delivery,
    ) -> std::result::Result<Waited, RecursionError> {
        let children = self
            .runtime
            .agents()
            .iter()
            .filter(|(_, entry)| entry.parent == Some(self.id))
            .filter_map(|(id, entry)| Some((*id, entry.outcome.clone()?)))
            .collect::<BTreeMap<_, _>>();
        let targets = match agents {
            Some(agents) => agents
                .iter()
                .map(|id| {
                    if children.contains_key(id) {
                        Ok(*id)
                    } else {
                        Err(RecursionError::InvalidRequest(format!(
                            "agent {id} is not a child of this agent"
                        )))
                    }
                })
                .collect::<std::result::Result<BTreeSet<_>, _>>()?,
            None => self
                .state
                .mailbox
                .outstanding()
                .into_iter()
                .filter(|id| children.contains_key(id))
                .collect(),
        };
        let until = timeout.map_or(self.deadline, |timeout| {
            Instant::now()
                .checked_add(timeout)
                .map_or(self.deadline, |until| until.min(self.deadline))
        });
        // Finished means the outcome is published and any notice has been queued.
        let finished =
            |id: &NodeId| children[id].borrow().is_some() && !self.state.mailbox.is_awaiting(*id);
        loop {
            let mut changed = self.state.mailbox.subscribe();
            self.check_open(&self.state.work.lock().expect("node work mutex poisoned"))?;
            if targets.iter().all(finished) {
                break;
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(RecursionError::Cancelled),
                _ = tokio::time::sleep_until(until) => break,
                _ = changed.changed() => {}
            }
        }
        let done = targets
            .iter()
            .copied()
            .filter(finished)
            .collect::<BTreeSet<_>>();
        let batch = {
            let mut work = self.state.work.lock().expect("node work mutex poisoned");
            self.check_open(&work)?;
            // One delivery of the finished children's messages, in arrival order. A
            // child's notice is its last message, so it never overtakes the others.
            let batch = self.state.mailbox.take(
                self.runtime.0.config.limits.delivery_chars,
                |message| done.contains(&message.from),
                |message| self.runtime.size(message),
            );
            if !batch.taken.is_empty() {
                self.record_delivery(&mut work, &batch.taken, via);
            }
            batch
        };
        Ok(Waited {
            messages: batch.taken,
            deferred: batch.left,
            finished: done
                .iter()
                .map(|id| {
                    children[id]
                        .borrow()
                        .clone()
                        .expect("finished child has an outcome")
                })
                .collect(),
            running: targets.difference(&done).copied().collect(),
        })
    }
    /// Cancels a descendant of this agent together with its subtree, through the
    /// ordered shutdown, and returns once it has stopped. For a direct child the
    /// terminal notice is taken here, so the canceller gets no separate message;
    /// a deeper descendant's notice still reaches its own parent. Cancelling an
    /// agent that already finished only reports its outcome.
    pub async fn cancel_agent(
        &self,
        agent: NodeId,
    ) -> std::result::Result<CancelOutcome, RecursionError> {
        self.check_open(&self.state.work.lock().expect("node work mutex poisoned"))?;
        let (cancel, mut outcome, parent) = {
            let agents = self.runtime.agents();
            let target = agents
                .get(&agent)
                .ok_or_else(|| RecursionError::InvalidRequest(format!("unknown agent: {agent}")))?;
            let mut ancestor = target.parent;
            while ancestor.is_some_and(|id| id != self.id) {
                ancestor = ancestor.and_then(|id| agents.get(&id)?.parent);
            }
            match (ancestor, &target.outcome) {
                (Some(_), Some(outcome)) if agent != self.id => {
                    (target.cancel.clone(), outcome.clone(), target.parent)
                }
                _ => {
                    return Err(RecursionError::InvalidRequest(format!(
                        "agent {agent} is not a descendant of this agent"
                    )));
                }
            }
        };
        let already_finished = outcome.borrow().is_some();
        cancel.cancel();
        if parent == Some(self.id) {
            let waited = self
                .wait_via(Some(&[agent]), None, Delivery::Cancel)
                .await?;
            return match waited.finished.into_iter().next() {
                Some(outcome) => Ok(CancelOutcome {
                    outcome,
                    already_finished,
                    remaining: waited.deferred.get(&agent).copied().unwrap_or_default(),
                    messages: waited.messages,
                }),
                // Only the deadline ends an untimed wait early.
                None => Err(RecursionError::Cancelled),
            };
        }
        loop {
            if let Some(outcome) = outcome.borrow_and_update().clone() {
                return Ok(CancelOutcome {
                    outcome,
                    already_finished,
                    messages: Vec::new(),
                    remaining: 0,
                });
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(RecursionError::Cancelled),
                _ = outcome.changed() => {}
            }
        }
    }
    /// Records the delivery of `messages` as owned work that this node's shutdown
    /// joins, after the node's earlier delivery records. The receiver turns true once
    /// the record is written.
    fn record_delivery(
        &self,
        work: &mut Work,
        messages: &[Envelope],
        via: Delivery,
    ) -> watch::Receiver<bool> {
        let (done, written) = watch::channel(false);
        let mut previous = work.recorded.replace(written.clone());
        let runtime = self.runtime.clone();
        let node = self.id;
        let messages = messages.iter().map(|message| message.id).collect();
        work.tasks.push(tokio::spawn(async move {
            if let Some(previous) = &mut previous {
                while !*previous.borrow_and_update() {
                    if previous.changed().await.is_err() {
                        break;
                    }
                }
            }
            // A failed trace write also fails node_end and so surfaces there.
            let _ = runtime
                .emit(TraceEvent::MessageDelivered {
                    node,
                    messages,
                    via,
                })
                .await;
            done.send_replace(true);
        }));
        written
    }
    /// Number of messages waiting in this agent's mailbox.
    pub fn pending_messages(&self) -> usize {
        self.state.mailbox.len()
    }
    /// Formats a message the way the runtime shows it to a model.
    pub fn render(&self, message: &Envelope) -> String {
        self.runtime.render(message)
    }
    /// Formats a finished child's outcome the way its terminal notice is shown.
    pub fn render_outcome(&self, outcome: &AgentOutcome) -> String {
        messages::render(
            MessageKind::for_status(outcome.status),
            outcome.node,
            &self.runtime.name(outcome.node),
            Some(outcome.status),
            &truncate(
                &outcome.answer.text(),
                self.runtime.0.config.limits.message_chars,
            ),
        )
    }
    fn cancel_status(&self) -> Status {
        if Instant::now() >= self.deadline {
            Status::Timeout
        } else {
            Status::Cancelled
        }
    }
    /// Returns current subtree headroom and accounting.
    pub fn budget(&self) -> crate::BudgetSnapshot {
        self.runtime.0.ledger.snapshot(self.id)
    }
    /// Admits and runs a traceable leaf completion owned by both node and caller tokens.
    /// Leaf calls consume the LLM count but no live-agent slot or agent depth.
    pub async fn llm(
        &self,
        call: LlmCall,
        owner: &CancellationToken,
    ) -> std::result::Result<LlmOutcome, RecursionError> {
        if call.max_tokens == Some(0) {
            return Err(RecursionError::InvalidRequest(
                "max_tokens must be positive".into(),
            ));
        }
        let scope = self.cancel.child_token();
        let _cancel_on_drop = CancelOnDrop(scope.clone());
        let owner = owner.clone();
        let model = call
            .model
            .clone()
            .unwrap_or_else(|| self.runtime.0.config.llm_model.clone());
        self.validate_model(&model)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut work = self.state.work.lock().expect("node work mutex poisoned");
            self.check_open(&work)?;
            if owner.is_cancelled() {
                return Err(RecursionError::Cancelled);
            }
            let id = self.runtime.0.ledger.admit(self.id, false, None)?;
            let cx = NodeCtx {
                id,
                parent: Some(self.id),
                depth: self.depth,
                cancel: scope.child_token(),
                deadline: self.deadline,
                model,
                runtime: self.runtime.clone(),
                state: Arc::new(NodeState::default()),
                cwd: self.cwd.clone(),
                options: call.options.clone(),
                owners: Arc::new(self.owners.iter().chain([&owner]).cloned().collect()),
            };
            // Own the task independently of the waiting future so settlement always completes.
            work.tasks.push(tokio::spawn(async move {
                let result = cx.llm_owned(call, owner).await;
                let _ = tx.send(result);
            }));
        }
        rx.await
            .map_err(|e| RecursionError::ModelError(e.to_string()))?
    }
    async fn llm_owned(
        &self,
        call: LlmCall,
        owner: CancellationToken,
    ) -> std::result::Result<LlmOutcome, RecursionError> {
        let cx = self;
        let id = self.id;
        let model = self.model.clone();
        let child = cx.cancel.clone();
        let watcher = tokio::spawn(async move {
            tokio::select! { _ = owner.cancelled() => child.cancel(), _ = child.cancelled() => {} }
        });
        let request = ModelRequest {
            model: model.model.clone(),
            system: call.system.clone(),
            messages: vec![Message::user_text(call.prompt.clone())],
            tools: vec![],
            max_tokens: call
                .max_tokens
                .unwrap_or(self.runtime.0.config.limits.llm_max_output_tokens)
                .min(self.runtime.0.config.limits.llm_max_output_tokens),
            options: call.options,
            metadata: RequestMeta {
                node_id: Some(id.to_string()),
                depth: self.depth,
            },
        };
        let result = AssertUnwindSafe(async {
            self.runtime
                .emit(TraceEvent::NodeStart {
                    node: id,
                    parent: self.parent,
                    depth: self.depth,
                    kind: "llm".into(),
                    name: String::new(),
                    model: model.to_string(),
                    origin_cell: None,
                    system: call.system,
                    tools: vec![],
                    limits: self.runtime.0.config.limits.clone(),
                    prompt: Some(Value::String(call.prompt)),
                })
                .await?;
            self.runtime.attempts(cx, &model, request, None).await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            self.runtime.0.panicked.store(true, Ordering::SeqCst);
            Err(CallError::Internal(anyhow::anyhow!("leaf task panicked")))
        });
        self.runtime.0.ledger.close_admission(id);
        cx.cancel.cancel();
        watcher.abort();
        let _ = watcher.await;
        let (status, text) = match &result {
            Ok((r, _)) => (
                match r.stop_reason {
                    StopReason::Refusal => Status::Refused,
                    StopReason::ModelContextWindowExceeded => Status::ContextExhausted,
                    StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                        Status::Completed
                    }
                    _ => Status::Failed,
                },
                Message {
                    role: Role::Assistant,
                    content: r.content.clone(),
                }
                .text(),
            ),
            Err(e) => (status_for(e, cx), String::new()),
        };
        let outcome = self
            .runtime
            .outcome(id, status, Answer::Text(text.clone()), 1);
        let emitted = self.runtime.emit(TraceEvent::NodeEnd { outcome }).await;
        self.runtime.0.ledger.shutdown(id);
        emitted.map_err(|e| RecursionError::ModelError(e.to_string()))?;
        let (response, _) = result.map_err(|error| match error {
            CallError::Budget => RecursionError::BudgetExceeded,
            CallError::Provider(ProviderError::Cancelled) => RecursionError::Cancelled,
            other => RecursionError::ModelError(other.to_string()),
        })?;
        if status != Status::Completed {
            return Err(RecursionError::ModelError(format!(
                "leaf ended with {status:?}"
            )));
        }
        Ok(LlmOutcome {
            node: id,
            text,
            response,
        })
    }
}
fn status_for(error: &CallError, cx: &NodeCtx) -> Status {
    match error {
        CallError::Budget => Status::BudgetExhausted,
        CallError::Provider(ProviderError::Cancelled) => cx.cancel_status(),
        CallError::Provider(
            ProviderError::ContextTooLarge(_) | ProviderError::Http { status: 413, .. },
        ) => Status::ContextExhausted,
        _ => Status::Failed,
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// Settlement is synchronous so unwinding or dropping an attempt future cannot
// leave tokens reserved. Once dispatch begins, unknown usage is charged in full.
struct ReservationGuard<'a> {
    ledger: &'a Ledger,
    reservation: Option<Reservation>,
    dispatched: bool,
}
impl<'a> ReservationGuard<'a> {
    fn new(ledger: &'a Ledger, reservation: Reservation) -> Self {
        Self {
            ledger,
            reservation: Some(reservation),
            dispatched: false,
        }
    }
    fn get(&self) -> &Reservation {
        self.reservation.as_ref().expect("unsettled reservation")
    }
    fn settle(mut self, charge: Charge) -> Settlement {
        self.ledger.settle(
            self.reservation.take().expect("unsettled reservation"),
            charge,
        )
    }
}
impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            let charge = if self.dispatched {
                AttemptCharge::Reserved
            } else {
                AttemptCharge::Zero
            };
            self.ledger.settle(reservation, Charge::Failed(charge));
        }
    }
}
