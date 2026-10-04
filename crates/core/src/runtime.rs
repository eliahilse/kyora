//! Sequential agent loop and the node-scoped entry point for leaf completions.
use crate::{
    Limits, ModelRef, ToolCx, ToolOutput, ToolSelection, ToolsetFactory, TraceEvent, TraceSink,
    defaults,
    ledger::{Charge, Ledger, NodeId, Reservation, Settlement, estimate},
    prompts,
    tool::truncate,
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
    collections::BTreeMap,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{sync::Semaphore, time::Instant};
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
    leaves: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
/// Shared runtime for one invocation. `run` may be called exactly once.
#[derive(Clone)]
pub struct Runtime(Arc<RuntimeInner>);
/// Node-scoped recursion context. Child-agent spawning is reserved for M1.3.
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
            leaves: std::sync::Mutex::new(Vec::new()),
        })))
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
        if self.0.started.swap(true, Ordering::SeqCst) {
            bail!("runtime already invoked");
        }
        let _cancel_on_drop = CancelOnDrop(self.0.cancel.clone());
        let runtime = self.clone();
        tokio::spawn(async move { runtime.run_owned(spec).await }).await?
    }
    async fn run_owned(&self, spec: AgentSpec) -> Result<AgentOutcome> {
        let deadline = Instant::now() + self.0.config.limits.run_timeout;
        let cx = NodeCtx {
            id: 0,
            parent: None,
            depth: 0,
            cancel: self.0.cancel.child_token(),
            deadline,
            model: spec.model.clone(),
            runtime: self.clone(),
        };
        let run_cancel = self.0.cancel.clone();
        let timer = tokio::spawn(async move {
            tokio::select! { _ = tokio::time::sleep_until(deadline) => run_cancel.cancel(), _ = run_cancel.cancelled() => {} }
        });
        let result = AssertUnwindSafe(async {
            self.emit(TraceEvent::SessionStart {
                session: self.0.config.session.clone(),
                cwd: spec.cwd.clone(),
                kyora: env!("CARGO_PKG_VERSION").into(),
                limits: self.0.config.limits.clone(),
            })
            .await?;
            self.agent(&cx, spec).await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            self.0.panicked.store(true, Ordering::SeqCst);
            Err(anyhow::anyhow!("root task panicked"))
        });
        cx.cancel.cancel();
        loop {
            let leaves =
                std::mem::take(&mut *self.0.leaves.lock().expect("leaf registry poisoned"));
            if leaves.is_empty() {
                break;
            }
            for leaf in leaves {
                if leaf.await.is_err() {
                    self.0.panicked.store(true, Ordering::SeqCst);
                }
            }
        }
        self.0.ledger.shutdown(0);
        timer.abort();
        let _ = timer.await;
        match result {
            Ok(mut outcome) => {
                if self.0.panicked.load(Ordering::SeqCst) {
                    outcome.status = Status::Failed;
                }
                (outcome.usage_self, outcome.usage_subtree) = self.0.ledger.usage(0);
                self.emit(TraceEvent::NodeEnd {
                    outcome: outcome.clone(),
                })
                .await?;
                self.emit(TraceEvent::SessionEnd {
                    status: outcome.status,
                })
                .await?;
                Ok(outcome)
            }
            Err(error) => {
                let outcome = self.outcome(0, Status::Failed, Answer::Text(String::new()), 0);
                let _ = self.emit(TraceEvent::NodeEnd { outcome }).await;
                let _ = self
                    .emit(TraceEvent::SessionEnd {
                        status: Status::Failed,
                    })
                    .await;
                Err(error)
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
    async fn agent(&self, cx: &NodeCtx, spec: AgentSpec) -> Result<AgentOutcome> {
        let info = NodeInfo {
            id: cx.id,
            parent: cx.parent,
            depth: cx.depth,
            model: cx.model.clone(),
            cwd: spec.cwd.clone(),
        };
        let tools = self.0.config.toolsets.toolset(&info, &spec.tools)?;
        let specs = tools.specs();
        let system = spec.system.unwrap_or_else(|| prompts::root(&specs));
        self.emit(TraceEvent::NodeStart {
            node: cx.id,
            parent: cx.parent,
            depth: cx.depth,
            kind: "agent".into(),
            name: "root".into(),
            model: spec.model.to_string(),
            system: Some(system.clone()),
            tools: specs.clone(),
            limits: self.0.config.limits.clone(),
            prompt: None,
        })
        .await?;
        let mut history = Vec::new();
        self.message(cx.id, &mut history, Message::user_text(spec.task))
            .await?;
        let mut turns = 0;
        let mut answer = Answer::Text(String::new());
        let mut previous = None;
        let mut output_cap = self.0.config.limits.max_output_tokens;
        let mut invalid_retry = false;
        let status = loop {
            if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
                break cx.cancel_status();
            }
            if turns >= self.0.config.limits.max_turns {
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
                } else if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
                    ToolOutput::error("cancelled")
                } else if let Err(error) = tools.validate(name, input) {
                    ToolOutput::error(error.to_string())
                } else {
                    let tool = tools.get(name).expect("validated tool");
                    let tool_cx = ToolCx {
                        node: cx.clone(),
                        call_id: id.into(),
                        cwd: spec.cwd.clone(),
                        cancel: cx.cancel.child_token(),
                        events: self.0.config.trace.clone(),
                    };
                    tokio::select! {
                        biased;
                        _ = cx.cancel.cancelled() => ToolOutput::error("cancelled"),
                        _ = tokio::time::sleep_until(cx.deadline) => { cx.cancel.cancel(); ToolOutput::error("cancelled") },
                        result = tool.call(input.clone(), tool_cx) => result,
                    }
                };
                let content = truncate(
                    &result.text_content(),
                    self.0.config.limits.tool_output_chars,
                );
                result.content = vec![ToolResultPart::Text {
                    text: content.clone(),
                }];
                if final_answer.is_none() {
                    final_answer = result.final_answer.take();
                }
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
                self.message(
                    cx.id,
                    &mut history,
                    Message {
                        role: Role::User,
                        content: results,
                    },
                )
                .await?;
            }
            if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
                break cx.cancel_status();
            }
            if let Some(value) = final_answer {
                answer = value;
                break Status::Completed;
            }
            match response.stop_reason {
                StopReason::ToolUse | StopReason::PauseTurn => {}
                StopReason::EndTurn => {
                    answer = Answer::Text(assistant.text());
                    break Status::Completed;
                }
                StopReason::MaxTokens => {
                    if !has_tools {
                        self.message(cx.id, &mut history, Message::user_text("continue"))
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
            if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
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
            let result = if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
                Err(ProviderError::NotSent("cancelled before dispatch".into()))
            } else {
                reservation.dispatched = true;
                tokio::select! {
                    biased;
                    _ = cx.cancel.cancelled() => Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep_until(cx.deadline) => Err(ProviderError::Cancelled),
                    result = tokio::time::timeout(self.0.config.limits.request_total, self.stream(cx, provider.as_ref(), request.clone(), &mut partial)) => result.unwrap_or(Err(ProviderError::IdleTimeout)),
                }
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
            match result {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if cx.cancel.is_cancelled() || Instant::now() >= cx.deadline {
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
    pub async fn llm(&self, call: LlmCall, owner: &CancellationToken) -> Result<LlmOutcome> {
        let scope = self.cancel.child_token();
        let _cancel_on_drop = CancelOnDrop(scope.clone());
        let node = self.clone();
        let owner = owner.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        // The owned task completes settlement even when a tool drops its waiting future.
        let task = tokio::spawn(async move {
            let result = node.llm_owned(call, &owner, scope).await;
            let _ = tx.send(result);
        });
        self.runtime
            .0
            .leaves
            .lock()
            .expect("leaf registry poisoned")
            .push(task);
        rx.await?
    }
    async fn llm_owned(
        &self,
        call: LlmCall,
        owner: &CancellationToken,
        scope: CancellationToken,
    ) -> Result<LlmOutcome> {
        if scope.is_cancelled() || owner.is_cancelled() || Instant::now() >= self.deadline {
            bail!("cancelled");
        }
        if call.max_tokens == Some(0) {
            bail!("max_tokens must be positive");
        }
        let model = call
            .model
            .unwrap_or_else(|| self.runtime.0.config.llm_model.clone());
        let id = self.runtime.0.ledger.admit(self.id, false, None)?;
        let cx = NodeCtx {
            id,
            parent: Some(self.id),
            depth: self.depth,
            cancel: scope.child_token(),
            deadline: self.deadline,
            model: model.clone(),
            runtime: self.runtime.clone(),
        };
        let child = cx.cancel.clone();
        let owner = owner.clone();
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
                    parent: Some(self.id),
                    depth: self.depth,
                    kind: "llm".into(),
                    name: String::new(),
                    model: model.to_string(),
                    system: call.system,
                    tools: vec![],
                    limits: self.runtime.0.config.limits.clone(),
                    prompt: Some(Value::String(call.prompt)),
                })
                .await?;
            self.runtime.attempts(&cx, &model, request, None).await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            self.runtime.0.panicked.store(true, Ordering::SeqCst);
            Err(CallError::Internal(anyhow::anyhow!("leaf task panicked")))
        });
        cx.cancel.cancel();
        watcher.abort();
        let _ = watcher.await;
        self.runtime.0.ledger.shutdown(id);
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
            Err(e) => (status_for(e, &cx), String::new()),
        };
        let outcome = self
            .runtime
            .outcome(id, status, Answer::Text(text.clone()), 1);
        self.runtime.emit(TraceEvent::NodeEnd { outcome }).await?;
        let (response, _) = result?;
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
