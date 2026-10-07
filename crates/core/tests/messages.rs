use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use kyora_core::{
    AgentOutcome, AgentSpec, Answer, ChildSpec, Delivery, Effect, Envelope, Limits, MessageId,
    MessageKind, NodeCtx, NodeId, Owner, RecursionError, Runtime, RuntimeConfig, Status, Tool,
    ToolCx, ToolOutput, Toolset, agent_tools,
    session::SessionStore,
    trace::{TraceEvent, TraceRecord, TraceSink, reconstruct_jsonl, reconstruct_tree},
};
use kyora_protocol::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolResultPart, ToolSpec,
    Usage,
};
use kyora_providers::{
    EventStream, ModelProvider, ProviderError, RetryPolicy,
    fake::{Matcher, Rule, ScriptedProvider},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    pin::pin,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct RustTool<F>(F);
#[async_trait]
impl<F> Tool for RustTool<F>
where
    F: Fn(Value, ToolCx) -> BoxFuture<'static, ToolOutput> + Send + Sync,
{
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "python".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        (self.0)(input, cx).await
    }
}
fn tool<F, Fut>(f: F) -> Arc<dyn Tool>
where
    F: Fn(Value, ToolCx) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ToolOutput> + Send + 'static,
{
    Arc::new(RustTool(move |value, cx| f(value, cx).boxed()))
}
fn idle_tool() -> Arc<dyn Tool> {
    tool(|_, _| async { ToolOutput::text("ok") })
}

/// Upper bound for every wait in these tests, so a lost message fails a test
/// instead of hanging it.
const LIMIT: Duration = Duration::from_secs(10);
/// Polls `ready` until it holds. Fails when the tool is cancelled first or after
/// `LIMIT`; a panicking tool fails its agent, which the test then reports.
async fn eventually(cx: &ToolCx, what: &str, ready: impl Fn() -> bool) {
    let until = tokio::time::Instant::now() + LIMIT;
    while !ready() {
        assert!(!cx.cancel.is_cancelled(), "cancelled waiting for {what}");
        assert!(
            tokio::time::Instant::now() < until,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A durable trace that acknowledges every record at once, except those `hold`
/// selects, whose acknowledgement waits for `release` (or `LIMIT`).
struct Acks {
    held: watch::Sender<usize>,
    open: watch::Sender<bool>,
}
impl Acks {
    fn new(hold: impl Fn(&TraceEvent) -> bool + Send + 'static) -> (TraceSink, Arc<Self>) {
        let acks = Arc::new(Self {
            held: watch::Sender::new(0),
            open: watch::Sender::new(false),
        });
        let state = acks.clone();
        let trace = TraceSink::with_store(move |record| {
            let state = state.clone();
            let hold = hold(&record.event) && !*state.open.borrow();
            async move {
                if hold {
                    state.held.send_modify(|held| *held += 1);
                    let mut open = state.open.subscribe();
                    let _ = tokio::time::timeout(LIMIT, async {
                        while !*open.borrow_and_update() {
                            if open.changed().await.is_err() {
                                break;
                            }
                        }
                    })
                    .await;
                }
                Ok(())
            }
        });
        (trace, acks)
    }
    /// Waits until `count` acknowledgements are being held.
    async fn held(&self, count: usize) {
        let mut held = self.held.subscribe();
        tokio::time::timeout(LIMIT, async {
            while *held.borrow_and_update() < count {
                held.changed().await.unwrap();
            }
        })
        .await
        .expect("held acknowledgements");
    }
    fn release(&self) {
        self.open.send_replace(true);
    }
}
/// Asserts that every accepted message got exactly one delivery or undelivered record.
fn assert_settled(records: &[TraceRecord]) {
    let mut ends = BTreeMap::<MessageId, usize>::new();
    for (_, messages, _) in delivered(records) {
        for id in messages {
            *ends.entry(id).or_default() += 1;
        }
    }
    for message in undelivered(records) {
        *ends.entry(message.id).or_default() += 1;
    }
    for message in sent(records) {
        assert_eq!(
            ends.get(&message.id),
            Some(&1),
            "message {} must end exactly once",
            message.id
        );
    }
}

type Pred = Box<dyn Fn(&ModelRequest) -> bool + Send + Sync>;
/// Scripted responses that record every request. A gated request is held until
/// a request matching its release predicate has been seen, or it is cancelled;
/// after `LIMIT` it fails instead.
struct Gated {
    scripted: ScriptedProvider,
    seen: watch::Sender<Vec<ModelRequest>>,
    gates: Vec<(Pred, Pred)>,
}
impl Gated {
    fn new(rules: Vec<Rule>, gates: Vec<(Pred, Pred)>) -> Arc<Self> {
        Arc::new(Self {
            scripted: ScriptedProvider::new(rules),
            seen: watch::Sender::new(Vec::new()),
            gates,
        })
    }
    async fn wait_for(&self, pred: Pred) {
        let mut seen = self.seen.subscribe();
        let until = tokio::time::Instant::now() + LIMIT;
        loop {
            let found = seen.borrow_and_update().iter().any(&pred);
            if found {
                return;
            }
            tokio::time::timeout_at(until, seen.changed())
                .await
                .expect("timed out waiting for a request")
                .unwrap();
        }
    }
    fn requested(&self, task: &str, turns: usize) -> bool {
        self.seen
            .borrow()
            .iter()
            .any(|request| at(task, turns)(request))
    }
    /// The request a conversation sent after `turns` assistant messages.
    fn request(&self, task: &str, turns: usize) -> ModelRequest {
        self.seen
            .borrow()
            .iter()
            .find(|request| at(task, turns)(request))
            .cloned()
            .unwrap_or_else(|| panic!("no request for {task:?} at turn {turns}"))
    }
}
#[async_trait]
impl ModelProvider for Gated {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        self.seen.send_modify(|seen| seen.push(request.clone()));
        for (gated, release) in &self.gates {
            if !gated(&request) {
                continue;
            }
            let mut seen = self.seen.subscribe();
            let until = tokio::time::Instant::now() + LIMIT;
            loop {
                let released = seen.borrow_and_update().iter().any(release);
                if released {
                    break;
                }
                tokio::select! {
                    _ = seen.changed() => {}
                    _ = cancel.cancelled() => return Err(ProviderError::cancelled(false)),
                    _ = tokio::time::sleep_until(until) => {
                        return Err(ProviderError::Other("gate was never released".into()));
                    }
                }
            }
        }
        self.scripted.stream(request, cancel).await
    }
}
fn first_user(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}
/// Matches the request a conversation sends after `turns` assistant messages.
fn at(task: &str, turns: usize) -> Pred {
    let task = task.to_owned();
    Box::new(move |request| {
        first_user(request).contains(&task)
            && request
                .messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .count()
                == turns
    })
}
fn never() -> Pred {
    Box::new(|_| false)
}
fn last(request: &ModelRequest) -> Message {
    request.messages.last().unwrap().clone()
}
fn texts(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}
fn results(message: &Message) -> Vec<(String, bool)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((
                content
                    .iter()
                    .map(|part| match part {
                        ToolResultPart::Text { text } => text.as_str(),
                    })
                    .collect(),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 7,
    }
}
fn text(text: &str) -> ModelResponse {
    ModelResponse {
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: usage(),
        id: None,
        model: String::new(),
        usage_iterations: vec![],
    }
}
fn calls(calls: Vec<(&str, Value)>) -> ModelResponse {
    ModelResponse {
        content: calls
            .into_iter()
            .enumerate()
            .map(|(index, (name, input))| ContentBlock::ToolUse {
                id: format!("t{index}"),
                name: name.into(),
                input,
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        ..text("")
    }
}
fn call(name: &str, input: Value) -> ModelResponse {
    calls(vec![(name, input)])
}
fn rule(task: &str, depth: u32, responses: Vec<ModelResponse>) -> Rule {
    Rule {
        matcher: Some(Matcher {
            first_user_contains: Some(task.into()),
            depth: Some(depth),
            ..Matcher::default()
        }),
        responses,
    }
}
fn setup(
    provider: Arc<dyn ModelProvider>,
    extra: Arc<dyn Tool>,
    limits: Limits,
    trace: TraceSink,
) -> Runtime {
    let mut tools = agent_tools::tools();
    tools.push(extra);
    Runtime::new(RuntimeConfig {
        providers: BTreeMap::from([("fake".into(), provider)]),
        toolsets: Arc::new(Toolset::new(tools).unwrap()),
        limits,
        retry: RetryPolicy {
            base: Duration::ZERO,
            ..RetryPolicy::default()
        },
        llm_model: "fake/leaf".parse().unwrap(),
        trace,
        session: "messages".into(),
    })
    .unwrap()
}
fn spec() -> AgentSpec {
    let mut spec = AgentSpec::new("root task", std::env::current_dir().unwrap());
    spec.model = "fake/agent".parse().unwrap();
    spec
}
fn records(mut rx: tokio::sync::broadcast::Receiver<TraceRecord>) -> Vec<TraceRecord> {
    let mut records = vec![];
    while let Ok(record) = rx.try_recv() {
        records.push(record);
    }
    records
}
fn sent(records: &[TraceRecord]) -> Vec<Envelope> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::MessageSent { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}
fn delivered(records: &[TraceRecord]) -> Vec<(NodeId, Vec<MessageId>, Delivery)> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::MessageDelivered {
                node,
                messages,
                via,
            } => Some((*node, messages.clone(), *via)),
            _ => None,
        })
        .collect()
}
fn undelivered(records: &[TraceRecord]) -> Vec<Envelope> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::MessageUndelivered { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}
fn position(records: &[TraceRecord], find: impl Fn(&TraceEvent) -> bool) -> usize {
    records
        .iter()
        .position(|record| find(&record.event))
        .expect("trace event present")
}
fn node_end(node: NodeId) -> impl Fn(&TraceEvent) -> bool {
    move |event| matches!(event, TraceEvent::NodeEnd { outcome } if outcome.node == node)
}
fn ended(records: &[TraceRecord], node: NodeId) -> AgentOutcome {
    records
        .iter()
        .find_map(|record| match &record.event {
            TraceEvent::NodeEnd { outcome } if outcome.node == node => Some(outcome.clone()),
            _ => None,
        })
        .expect("node ended")
}
fn status(records: &[TraceRecord], node: NodeId) -> Status {
    ended(records, node).status
}
fn tool_names(records: &[TraceRecord], node: NodeId) -> Vec<String> {
    records
        .iter()
        .find_map(|record| match &record.event {
            TraceEvent::NodeStart {
                node: id, tools, ..
            } if *id == node => Some(tools.iter().map(|tool| tool.name.clone()).collect()),
            _ => None,
        })
        .expect("node started")
}
fn dates_schema() -> Value {
    json!({"type":"object","properties":{"dates":{"type":"array","items":{"type":"string"}}},"required":["dates"],"additionalProperties":false})
}

#[tokio::test]
async fn parent_keeps_working_while_child_runs_and_result_arrives_as_message() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::create(home.path()).unwrap();
    let rx = store.trace.subscribe();
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    text("waiting"),
                    text("done"),
                ],
            ),
            rule("child task", 1, vec![text("child answer")]),
        ],
        // The child cannot answer before the parent has made its next request.
        vec![(at("child task", 0), at("root task", 1))],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits::default(),
        store.trace.clone(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    store.trace.finish().await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.answer.text(), "done");
    assert_eq!(outcome.turns, 3);
    let spawned = provider.request("root task", 1);
    assert_eq!(
        results(&last(&spawned)),
        vec![("started agent 1 (worker)".into(), false)]
    );
    assert!(texts(&last(&spawned)).is_empty());
    // The idle parent woke up with the result as a separate user message.
    let woken = provider.request("root task", 2);
    assert_eq!(last(&woken).role, Role::User);
    assert_eq!(
        texts(&last(&woken)),
        vec!["[result from agent 1 (worker): completed]\nchild answer"]
    );
    let live = records(rx);
    let messages = sent(&live);
    assert_eq!(messages.len(), 1);
    let result = &messages[0];
    assert_eq!(
        (
            result.from,
            result.to,
            result.kind,
            result.spawn,
            result.status
        ),
        (1, 0, MessageKind::Result, Some(1), Some(Status::Completed))
    );
    assert_eq!(result.body, "child answer");
    assert_eq!(delivered(&live), vec![(0, vec![result.id], Delivery::Turn)]);
    let child_end = position(&live, node_end(1));
    let send = position(&live, |event| {
        matches!(event, TraceEvent::MessageSent { .. })
    });
    let delivery = position(&live, |event| {
        matches!(event, TraceEvent::MessageDelivered { .. })
    });
    assert!(child_end < send && send < delivery && delivery < position(&live, node_end(0)));
    // Message records persist and do not disturb tree reconstruction.
    let written = std::fs::read_to_string(store.path.join("events.jsonl")).unwrap();
    assert!(written.contains("\"type\":\"message_sent\""));
    assert!(written.contains("\"type\":\"message_delivered\""));
    let tree = reconstruct_jsonl(&written).unwrap();
    assert_eq!(tree, reconstruct_tree(&live).unwrap());
    assert_eq!(tree[0].children[0].status, Some(Status::Completed));
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn follow_up_reaches_child_at_its_next_turn_boundary() {
    let probe = tool(|_, cx| async move {
        // Hold the child inside a tool call until the follow-up is queued.
        eventually(&cx, "the follow-up", || cx.node.pending_messages() > 0).await;
        ToolOutput::text("probed")
    });
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    call(
                        "send_message",
                        json!({"to": "worker", "body": "also check B"}),
                    ),
                    text("waiting"),
                    text("done"),
                ],
            ),
            rule(
                "child task",
                1,
                vec![call("python", json!({})), text("checked B")],
            ),
        ],
        vec![
            // The parent sends only after the child's first message is built, and the
            // child finishes only after the parent's next boundary.
            (at("root task", 1), at("child task", 0)),
            (at("child task", 1), at("root task", 2)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), probe, Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![("sent message 0 to agent 1".into(), false)]
    );
    assert!(
        !provider
            .request("child task", 0)
            .messages
            .iter()
            .any(|message| message.text().contains("also check B"))
    );
    // The follow-up comes after the tool result in the same user message.
    let next = provider.request("child task", 1);
    let boundary = last(&next);
    assert_eq!(boundary.role, Role::User);
    assert!(matches!(
        &boundary.content[..],
        [ContentBlock::ToolResult { tool_use_id, .. }, ContentBlock::Text { text }]
            if tool_use_id == "t0" && text == "[message from agent 0 (root)]\nalso check B"
    ));
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec!["[result from agent 1 (worker): completed]\nchecked B"]
    );
    let live = records(rx);
    let follow_up = &sent(&live)[0];
    assert_eq!(
        (
            follow_up.from,
            follow_up.to,
            follow_up.kind,
            follow_up.spawn
        ),
        (0, 1, MessageKind::Message, Some(1))
    );
    assert_eq!(delivered(&live)[0], (1, vec![follow_up.id], Delivery::Turn));
}

#[tokio::test]
async fn child_reports_progress_and_keeps_running() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    call("receive", json!({"yield_after": 30})),
                    text("waiting"),
                    text("done"),
                ],
            ),
            rule(
                "child task",
                1,
                vec![
                    call("send_message", json!({"to": "parent", "body": "half done"})),
                    text("all done"),
                ],
            ),
        ],
        vec![
            (at("child task", 0), at("root task", 1)),
            // The child cannot finish until the parent has acted on its progress.
            (at("child task", 1), at("root task", 2)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    let progress = last(&provider.request("root task", 2));
    assert_eq!(
        results(&progress),
        vec![("[message from agent 1 (worker)]\nhalf done".into(), false)]
    );
    assert!(texts(&progress).is_empty());
    assert_eq!(
        results(&last(&provider.request("child task", 1))),
        vec![("sent message 0 to agent 0".into(), false)]
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec!["[result from agent 1 (worker): completed]\nall done"]
    );
    let live = records(rx);
    let messages = sent(&live);
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.from, m.to, m.kind, m.spawn))
            .collect::<Vec<_>>(),
        vec![
            (1, 0, MessageKind::Message, Some(1)),
            (1, 0, MessageKind::Result, Some(1)),
        ]
    );
    assert_eq!(
        delivered(&live),
        vec![
            (0, vec![messages[0].id], Delivery::Receive),
            (0, vec![messages[1].id], Delivery::Turn),
        ]
    );
    // The progress message was delivered while the child was still running.
    assert!(
        position(&live, |event| matches!(
            event,
            TraceEvent::MessageDelivered {
                via: Delivery::Receive,
                ..
            }
        )) < position(&live, node_end(1))
    );
}

#[tokio::test]
async fn full_mailbox_refuses_plain_messages_but_never_a_result() {
    let python = tool(|_, cx| async move {
        if cx.node.depth == 0 {
            // Two progress messages and the result notice, which ignores the bound.
            eventually(&cx, "two messages and a result", || {
                cx.node.pending_messages() == 3
            })
            .await;
            return ToolOutput::text("ok");
        }
        let parent = cx.node.resolve("parent").unwrap();
        cx.node.send(parent, "p1").await.unwrap();
        cx.node.send(parent, "p2").await.unwrap();
        match cx.node.send(parent, "p3").await {
            Err(error @ RecursionError::MailboxFull { agent: 0 }) => {
                ToolOutput::error(error.to_string())
            }
            other => panic!("unexpected send result: {other:?}"),
        }
    });
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    call("python", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "child task",
                1,
                vec![call("python", json!({})), text("child done")],
            ),
        ],
        vec![(at("child task", 0), at("root task", 1))],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider.clone(),
        python,
        Limits {
            mailbox_capacity: 2,
            ..Limits::default()
        },
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(
        results(&last(&provider.request("child task", 1))),
        vec![("mailbox of agent 0 is full".into(), true)]
    );
    // Delivered in arrival order: per-sender order holds across kinds.
    assert_eq!(
        texts(&last(&provider.request("root task", 2))),
        vec![
            "[message from agent 1 (worker)]\np1",
            "[message from agent 1 (worker)]\np2",
            "[result from agent 1 (worker): completed]\nchild done",
        ]
    );
    let live = records(rx);
    assert_eq!(
        sent(&live)
            .iter()
            .map(|m| (m.kind, m.body.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (MessageKind::Message, "p1"),
            (MessageKind::Message, "p2"),
            (MessageKind::Result, "child done"),
        ]
    );
    assert!(undelivered(&live).is_empty());
}

#[tokio::test]
async fn messages_to_a_finished_agent_fail_for_the_sender() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    call("wait", json!({})),
                    calls(vec![
                        ("send_message", json!({"to": "worker", "body": "late"})),
                        ("send_message", json!({"to": "nobody", "body": "lost"})),
                        ("send_message", json!({"to": "#0", "body": "self"})),
                    ]),
                    text("done"),
                ],
            ),
            rule("child task", 1, vec![text("child done")]),
        ],
        vec![(at("child task", 0), at("root task", 1))],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    // Wait returned the result and took its notice, so nothing else was injected.
    let waited = last(&provider.request("root task", 2));
    assert_eq!(
        results(&waited),
        vec![(
            "[result from agent 1 (worker): completed]\nchild done".into(),
            false
        )]
    );
    assert!(texts(&waited).is_empty());
    assert_eq!(
        results(&last(&provider.request("root task", 3))),
        vec![
            ("agent 1 has finished".into(), true),
            ("invalid request: unknown agent: nobody".into(), true),
            (
                "invalid request: cannot send a message to yourself".into(),
                true
            ),
        ]
    );
    let live = records(rx);
    let messages = sent(&live);
    assert_eq!(messages.len(), 1);
    assert_eq!(
        delivered(&live),
        vec![(0, vec![messages[0].id], Delivery::Wait)]
    );
    assert!(undelivered(&live).is_empty());
}

#[tokio::test]
async fn shutdown_records_undelivered_messages_and_cancels_children() {
    let saved = Arc::new(Mutex::new(None::<NodeCtx>));
    let saved_tool = saved.clone();
    let provider = Gated::new(
        vec![rule("root task", 0, vec![call("python", json!({}))])],
        vec![(at("blocked child", 0), never())],
    );
    let started = provider.clone();
    let python = tool(move |_, cx| {
        let saved = saved_tool.clone();
        let started = started.clone();
        async move {
            let child = cx
                .node
                .spawn_agent(
                    ChildSpec {
                        name: Some("worker".into()),
                        ..ChildSpec::new("blocked child")
                    },
                    Owner::Node,
                )
                .unwrap();
            // The child's first message is built, so both messages stay queued.
            started.wait_for(at("blocked child", 0)).await;
            cx.node.send(child.id, "first").await.unwrap();
            cx.node.send(child.id, "second").await.unwrap();
            assert_eq!(child.status().status, None);
            *saved.lock().unwrap() = Some(cx.node.clone());
            let mut result = ToolOutput::text("ok");
            result.final_answer = Some(Answer::Text("final".into()));
            result
        }
    });
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider, python, Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let live = records(rx);
    assert_eq!(status(&live, 1), Status::Cancelled);
    assert!(delivered(&live).is_empty());
    let lost = undelivered(&live);
    assert_eq!(
        lost.iter()
            .map(|m| (m.from, m.to, m.kind, m.body.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (0, 1, MessageKind::Message, "first"),
            (0, 1, MessageKind::Message, "second"),
            (1, 0, MessageKind::Cancelled, ""),
        ]
    );
    assert_eq!(lost[2].status, Some(Status::Cancelled));
    // Queued messages are recorded before the child ends, its notice after it
    // ended and before its parent ends.
    let lost_at = live
        .iter()
        .enumerate()
        .filter(|(_, record)| matches!(record.event, TraceEvent::MessageUndelivered { .. }))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let child_end = position(&live, node_end(1));
    assert!(lost_at[1] < child_end && child_end < lost_at[2]);
    assert!(lost_at[2] < position(&live, node_end(0)));
    // A finished agent can no longer send.
    let node = saved.lock().unwrap().take().unwrap();
    assert!(matches!(
        node.send(1, "late").await,
        Err(RecursionError::Cancelled)
    ));
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn cell_owned_children_post_no_notice_and_node_owned_cancellation_is_reported() {
    let python = tool(|_, cx| async move {
        let cell = CancellationToken::new();
        let scoped = cx
            .node
            .spawn_agent(
                ChildSpec::new("blocked cell child"),
                Owner::Cell(cell.clone()),
            )
            .unwrap();
        let persistent = cx
            .node
            .spawn_agent(ChildSpec::new("blocked node child"), Owner::Node)
            .unwrap();
        // Timeouts return what is known so far instead of failing.
        assert!(
            cx.node
                .receive(Duration::from_millis(20))
                .await
                .unwrap()
                .is_empty()
        );
        let waited = cx
            .node
            .wait(Some(&[persistent.id]), Some(Duration::from_millis(20)))
            .await
            .unwrap();
        assert!(waited.finished.is_empty());
        assert_eq!(waited.running, vec![persistent.id]);
        cell.cancel();
        assert_eq!(scoped.result().await.status, Status::Cancelled);
        persistent.cancel();
        let waited = cx
            .node
            .wait(Some(&[scoped.id, persistent.id]), None)
            .await
            .unwrap();
        assert_eq!(
            waited
                .finished
                .iter()
                .map(|outcome| (outcome.node, outcome.status))
                .collect::<Vec<_>>(),
            vec![
                (scoped.id, Status::Cancelled),
                (persistent.id, Status::Cancelled)
            ]
        );
        assert!(waited.running.is_empty());
        assert_eq!(cx.node.pending_messages(), 0);
        ToolOutput::text("ok")
    });
    let provider = Gated::new(
        vec![rule(
            "root task",
            0,
            vec![call("python", json!({})), text("done")],
        )],
        vec![(at("blocked", 0), never())],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider, python, Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 2));
    let live = records(rx);
    let messages = sent(&live);
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.from, m.kind, m.status))
            .collect::<Vec<_>>(),
        vec![(2, MessageKind::Cancelled, Some(Status::Cancelled))]
    );
    assert_eq!(
        delivered(&live),
        vec![(0, vec![messages[0].id], Delivery::Wait)]
    );
    assert!(undelivered(&live).is_empty());
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn idle_parent_waits_for_each_child_result() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        ("spawn_agent", json!({"task": "child a", "name": "a"})),
                        ("spawn_agent", json!({"task": "child b", "name": "b"})),
                    ]),
                    text("waiting"),
                    text("got a"),
                    text("done"),
                ],
            ),
            rule("child a", 1, vec![text("answer a")]),
            rule("child b", 1, vec![text("answer b")]),
        ],
        vec![
            (at("child a", 0), at("root task", 1)),
            (at("child b", 0), at("root task", 2)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.answer.text(), "done");
    assert_eq!(outcome.turns, 4);
    assert_eq!(
        texts(&last(&provider.request("root task", 2))),
        vec!["[result from agent 1 (a): completed]\nanswer a"]
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec!["[result from agent 2 (b): completed]\nanswer b"]
    );
    let live = records(rx);
    // Both children finished on their own; the parent did not cancel them.
    assert_eq!(status(&live, 1), Status::Completed);
    assert_eq!(status(&live, 2), Status::Completed);
    let root_end = position(&live, node_end(0));
    assert!(position(&live, node_end(1)) < root_end && position(&live, node_end(2)) < root_end);
}

#[tokio::test]
async fn injected_messages_count_toward_the_next_reservation() {
    let mut reserved = vec![];
    for size in [10, 1010] {
        let body = "x".repeat(size);
        let provider = Gated::new(
            vec![
                rule(
                    "root task",
                    0,
                    vec![
                        call(
                            "spawn_agent",
                            json!({"task": "child task", "name": "worker"}),
                        ),
                        text("waiting"),
                        text("still waiting"),
                        text("done"),
                    ],
                ),
                rule(
                    "child task",
                    1,
                    vec![
                        call("send_message", json!({"to": "parent", "body": body})),
                        text("child done"),
                    ],
                ),
            ],
            vec![
                (at("child task", 0), at("root task", 1)),
                (at("child task", 1), at("root task", 2)),
            ],
        );
        let trace = TraceSink::ephemeral();
        let rx = trace.subscribe();
        let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
        let outcome = runtime.run(spec()).await.unwrap();
        assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
        assert_eq!(
            texts(&last(&provider.request("root task", 2))),
            vec![format!("[message from agent 1 (worker)]\n{body}")]
        );
        // Messages cost nothing by themselves: four root turns, two child turns.
        assert_eq!(outcome.usage_self.total(), 4 * 130);
        assert_eq!(outcome.usage_subtree.total(), 6 * 130);
        let live = records(rx);
        let charged = live
            .iter()
            .filter_map(|record| match record.event {
                TraceEvent::AttemptEnd { charged, .. } => Some(charged),
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(charged, outcome.usage_subtree.total());
        assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
        reserved.push(
            live.iter()
                .filter_map(|record| match record.event {
                    TraceEvent::AttemptStart {
                        node: 0, reserved, ..
                    } => Some(reserved),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    // Only the request that carries the message reserves more, by its extra bytes.
    assert_eq!(reserved[0].len(), 4);
    assert_eq!(reserved[0][..2], reserved[1][..2]);
    assert_eq!(reserved[1][2] - reserved[0][2], 1000);
    assert_eq!(reserved[0][3], reserved[1][3]);
}

#[tokio::test]
async fn siblings_exchange_messages_by_name() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        ("spawn_agent", json!({"task": "child a", "name": "a"})),
                        ("spawn_agent", json!({"task": "child b", "name": "b"})),
                    ]),
                    call("wait", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "child a",
                1,
                vec![
                    call("send_message", json!({"to": "b", "body": "hello from a"})),
                    text("a done"),
                ],
            ),
            rule(
                "child b",
                1,
                vec![call("receive", json!({"yield_after": 30})), text("b done")],
            ),
        ],
        // a sends only once b's first message is built, so b receives it mid-turn.
        vec![
            (at("child b", 0), at("root task", 1)),
            (at("child a", 0), at("child b", 0)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 3));
    assert_eq!(
        results(&last(&provider.request("child b", 1))),
        vec![("[message from agent 1 (a)]\nhello from a".into(), false)]
    );
    // Results come back in the order the siblings finished, which either may win.
    let waited = results(&last(&provider.request("root task", 2)));
    let mut parts = waited[0].0.split("\n\n").collect::<Vec<_>>();
    parts.sort();
    assert_eq!(
        parts,
        vec![
            "[result from agent 1 (a): completed]\na done",
            "[result from agent 2 (b): completed]\nb done",
        ]
    );
    assert!(!waited[0].1);
    let live = records(rx);
    let greeting = &sent(&live)[0];
    assert_eq!((greeting.from, greeting.to, greeting.spawn), (1, 2, None));
}

#[tokio::test]
async fn addressing_is_limited_to_parent_children_and_siblings() {
    let python = tool(|_, cx| async move {
        let node = &cx.node;
        match node.depth {
            0 => {
                assert!(matches!(
                    node.resolve("parent"),
                    Err(RecursionError::InvalidRequest(_))
                ));
                let mut children = vec![];
                for (task, name) in [("child x", "x"), ("child y", "y"), ("child twin", "twin")] {
                    children.push(
                        node.spawn_agent(
                            ChildSpec {
                                name: Some(name.into()),
                                ..ChildSpec::new(task)
                            },
                            Owner::Node,
                        )
                        .unwrap(),
                    );
                }
                children.push(
                    node.spawn_agent(
                        ChildSpec {
                            name: Some("twin".into()),
                            ..ChildSpec::new("child twin")
                        },
                        Owner::Node,
                    )
                    .unwrap(),
                );
                assert_eq!(node.resolve("x").unwrap(), children[0].id);
                assert!(matches!(
                    node.resolve("twin"),
                    Err(RecursionError::InvalidRequest(message)) if message.contains("ambiguous")
                ));
                let ids = children.iter().map(|child| child.id).collect::<Vec<_>>();
                let waited = node.wait(Some(&ids), None).await.unwrap();
                assert_eq!(waited.finished.len(), 4);
                assert!(matches!(
                    node.send(children[0].id, "late").await,
                    Err(RecursionError::AgentFinished { agent: 1 })
                ));
                ToolOutput::text("ok")
            }
            // Only x has the python tool script; it spawns a grandchild.
            1 => {
                let grandchild = node
                    .spawn_agent(ChildSpec::new("grandchild"), Owner::Node)
                    .unwrap();
                grandchild.result().await;
                ToolOutput::text("ok")
            }
            _ => {
                let parent = node.resolve("parent").unwrap();
                assert_eq!(parent, 1);
                assert_eq!(node.resolve("x").unwrap(), parent);
                assert_eq!(node.resolve(" #1 ").unwrap(), parent);
                // An uncle is not addressable by name or by id.
                assert!(matches!(
                    node.resolve("y"),
                    Err(RecursionError::InvalidRequest(_))
                ));
                assert!(matches!(
                    node.send(node.resolve("2").unwrap(), "hi").await,
                    Err(RecursionError::InvalidRequest(message)) if message.contains("not this agent's parent")
                ));
                assert!(matches!(
                    node.send(0, "hi").await,
                    Err(RecursionError::InvalidRequest(_))
                ));
                assert!(matches!(
                    node.send(99, "hi").await,
                    Err(RecursionError::InvalidRequest(message)) if message == "unknown agent: 99"
                ));
                assert!(matches!(
                    node.send(parent, "x".repeat(21)).await,
                    Err(RecursionError::InvalidRequest(_))
                ));
                node.send(parent, "hi").await.unwrap();
                ToolOutput::text("ok")
            }
        }
    });
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![call("python", json!({})), text("done")],
            ),
            rule(
                "child x",
                1,
                vec![call("python", json!({})), text("x done")],
            ),
            rule("child", 1, vec![text("child done")]),
            rule(
                "grandchild",
                2,
                vec![call("python", json!({})), text("grandchild done")],
            ),
        ],
        vec![],
    );
    let runtime = setup(
        provider,
        python,
        Limits {
            message_chars: 20,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
}

#[tokio::test]
async fn cancel_agent_stops_a_descendant_and_its_subtree() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "worker task", "name": "worker"}),
                    ),
                    call("cancel_agent", json!({"to": "2"})),
                    call("cancel_agent", json!({"to": "worker"})),
                    text("done"),
                ],
            ),
            rule(
                "worker task",
                1,
                vec![
                    calls(vec![
                        ("spawn_agent", json!({"task": "deep task", "name": "deep"})),
                        (
                            "spawn_agent",
                            json!({"task": "deep task two", "name": "deep2"}),
                        ),
                    ]),
                    text("waiting"),
                ],
            ),
        ],
        vec![
            // The root cancels the grandchild once both grandchildren exist, and the
            // worker once it has heard about that.
            (at("root task", 1), at("worker task", 1)),
            (at("root task", 2), at("worker task", 2)),
            (at("worker task", 2), never()),
            (at("deep task", 0), never()),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    // Cancelling a grandchild: the canceller gets the outcome, its parent the notice.
    let first = last(&provider.request("root task", 2));
    assert_eq!(
        results(&first),
        vec![("[cancelled from agent 2 (deep): cancelled]".into(), false)]
    );
    assert!(texts(&first).is_empty());
    assert_eq!(
        texts(&last(&provider.request("worker task", 2))),
        vec!["[cancelled from agent 2 (deep): cancelled]"]
    );
    // Cancelling a direct child: the outcome comes back and no separate message follows.
    let second = last(&provider.request("root task", 3));
    assert_eq!(
        results(&second),
        vec![(
            "[cancelled from agent 1 (worker): cancelled]\nwaiting".into(),
            false
        )]
    );
    assert!(texts(&second).is_empty());
    let live = records(rx);
    for node in [1, 2, 3] {
        assert_eq!(status(&live, node), Status::Cancelled);
    }
    let notices = sent(&live);
    let id = |from| notices.iter().find(|m| m.from == from).unwrap().id;
    assert_eq!(
        delivered(&live),
        vec![
            (1, vec![id(2)], Delivery::Turn),
            (0, vec![id(1)], Delivery::Cancel),
        ]
    );
    // deep2 was cancelled with the worker; its notice found the worker closed.
    let lost = undelivered(&live);
    assert_eq!(
        lost.iter()
            .map(|m| (m.from, m.to, m.kind))
            .collect::<Vec<_>>(),
        vec![(3, 1, MessageKind::Cancelled)]
    );
    let worker_end = position(&live, node_end(1));
    assert!(position(&live, node_end(2)) < worker_end);
    assert!(position(&live, node_end(3)) < worker_end);
    assert!(worker_end < position(&live, node_end(0)));
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn cancel_agent_reports_a_finished_child_and_refuses_non_descendants() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        (
                            "spawn_agent",
                            json!({"task": "child task", "name": "worker"}),
                        ),
                        ("spawn_agent", json!({"task": "peer task", "name": "peer"})),
                    ]),
                    call("wait", json!({})),
                    calls(vec![
                        ("cancel_agent", json!({"to": "worker"})),
                        ("cancel_agent", json!({"to": "worker"})),
                    ]),
                    text("done"),
                ],
            ),
            rule(
                "child task",
                1,
                vec![
                    calls(vec![
                        ("cancel_agent", json!({"to": "parent"})),
                        ("cancel_agent", json!({"to": "#1"})),
                        ("cancel_agent", json!({"to": "peer"})),
                    ]),
                    text("child done"),
                ],
            ),
            rule("peer task", 1, vec![text("peer done")]),
        ],
        vec![
            (at("child task", 0), at("root task", 1)),
            (at("peer task", 0), at("root task", 1)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    assert_eq!(
        results(&last(&provider.request("child task", 1))),
        vec![
            (
                "invalid request: agent 0 is not a descendant of this agent".into(),
                true
            ),
            (
                "invalid request: agent 1 is not a descendant of this agent".into(),
                true
            ),
            (
                "invalid request: agent 2 is not a descendant of this agent".into(),
                true
            ),
        ]
    );
    let finished =
        "agent 1 had already finished\n[result from agent 1 (worker): completed]\nchild done";
    assert_eq!(
        results(&last(&provider.request("root task", 3))),
        vec![(finished.into(), false), (finished.into(), false)]
    );
    let live = records(rx);
    assert_eq!(status(&live, 1), Status::Completed);
    assert_eq!(status(&live, 2), Status::Completed);
    // Both results were taken by wait; the cancel calls took nothing.
    assert!(
        delivered(&live)
            .iter()
            .all(|(node, _, via)| (*node, *via) == (0, Delivery::Wait))
    );
}

#[tokio::test]
async fn submitting_a_result_ends_the_child_and_cancels_its_children() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "extract task", "name": "extract", "output": dates_schema()}),
                    ),
                    text("waiting"),
                    text("done"),
                ],
            ),
            rule(
                "extract task",
                1,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "helper task", "name": "helper"}),
                    ),
                    call("submit_result", json!({"dates": ["2026-10-07"]})),
                ],
            ),
        ],
        vec![
            (at("extract task", 0), at("root task", 1)),
            // The child submits while its helper is still running.
            (at("extract task", 1), at("helper task", 0)),
            (at("helper task", 0), never()),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 3));
    let live = records(rx);
    assert!(tool_names(&live, 1).contains(&"submit_result".to_string()));
    assert!(!tool_names(&live, 0).contains(&"submit_result".to_string()));
    let submit = live
        .iter()
        .find_map(|record| match &record.event {
            TraceEvent::NodeStart { node: 1, tools, .. } => tools
                .iter()
                .find(|tool| tool.name == "submit_result")
                .cloned(),
            _ => None,
        })
        .unwrap();
    assert_eq!(submit.input_schema, dates_schema());
    // The child ended at once with the structured answer and cancelled its helper.
    let extract = ended(&live, 1);
    assert_eq!((extract.status, extract.turns), (Status::Completed, 2));
    assert!(matches!(&extract.answer, Answer::Value(_)));
    assert!(!provider.requested("extract task", 2));
    assert_eq!(status(&live, 2), Status::Cancelled);
    assert!(position(&live, node_end(2)) < position(&live, node_end(1)));
    let notice = sent(&live)
        .into_iter()
        .find(|m| m.from == 1 && m.kind == MessageKind::Result)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&notice.body).unwrap(),
        json!({"dates": ["2026-10-07"]})
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 2))),
        vec!["[result from agent 1 (extract): completed]\n{\"dates\":[\"2026-10-07\"]}"]
    );
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn invalid_output_schemas_and_submissions_return_what_to_fix() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        (
                            "spawn_agent",
                            json!({"task": "scalar", "output": {"type": "string"}}),
                        ),
                        (
                            "spawn_agent",
                            json!({"task": "union", "output": {"type": "object", "properties": {"x": {"type": ["string", "null"]}}}}),
                        ),
                        (
                            "spawn_agent",
                            json!({"task": "extract task", "name": "extract", "output": dates_schema()}),
                        ),
                    ]),
                    call("wait", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "extract task",
                1,
                vec![
                    call("submit_result", json!({"dates": "2026-10-07"})),
                    call(
                        "submit_result",
                        json!({"dates": ["2026-10-07"], "note": "x"}),
                    ),
                    call("submit_result", json!({"dates": ["2026-10-07"]})),
                ],
            ),
        ],
        vec![(at("extract task", 0), at("root task", 1))],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 3));
    assert_eq!(
        results(&last(&provider.request("root task", 1))),
        vec![
            (
                "invalid request: output must be a JSON schema of type object".into(),
                true
            ),
            (
                "invalid request: invalid output schema: unsupported schema type: [\"string\",\"null\"]"
                    .into(),
                true
            ),
            ("started agent 1 (extract)".into(), false),
        ]
    );
    assert_eq!(
        results(&last(&provider.request("extract task", 1))),
        vec![(
            "result does not match the output schema: invalid input type, expected \"array\""
                .into(),
            true
        )]
    );
    assert_eq!(
        results(&last(&provider.request("extract task", 2))),
        vec![(
            "result does not match the output schema: unexpected property: note".into(),
            true
        )]
    );
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![(
            "[result from agent 1 (extract): completed]\n{\"dates\":[\"2026-10-07\"]}".into(),
            false
        )]
    );
    let live = records(rx);
    // Submissions are checked by the tool, so the model sees the reason.
    assert!(
        !live
            .iter()
            .any(|record| matches!(record.event, TraceEvent::InvalidToolInput { .. }))
    );
    assert_eq!(ended(&live, 1).turns, 3);
}

#[tokio::test]
async fn ending_a_turn_without_submitting_is_reminded_once_then_fails() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        (
                            "spawn_agent",
                            json!({"task": "lazy task", "name": "lazy", "output": dates_schema()}),
                        ),
                        (
                            "spawn_agent",
                            json!({"task": "late task", "name": "late", "output": dates_schema()}),
                        ),
                    ]),
                    call("wait", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "lazy task",
                1,
                vec![text("no dates here"), text("still none")],
            ),
            rule(
                "late task",
                1,
                vec![
                    text("thinking"),
                    call("submit_result", json!({"dates": []})),
                ],
            ),
        ],
        vec![
            (at("lazy task", 0), at("root task", 1)),
            (at("late task", 0), at("root task", 1)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 3));
    for task in ["lazy task", "late task"] {
        assert_eq!(
            texts(&last(&provider.request(task, 1))),
            vec![agent_tools::SUBMIT_REMINDER]
        );
    }
    assert!(!provider.requested("lazy task", 2));
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![(
            "[error from agent 1 (lazy): failed]\nended without calling submit_result. Last reply: still none\n\n[result from agent 2 (late): completed]\n{\"dates\":[]}"
                .into(),
            false
        )]
    );
    let live = records(rx);
    assert_eq!(
        (status(&live, 1), ended(&live, 1).turns),
        (Status::Failed, 2)
    );
    assert_eq!(status(&live, 2), Status::Completed);
    assert!(live.iter().any(|record| matches!(
        &record.event,
        TraceEvent::Error { node: 1, message } if message == "ended without calling submit_result"
    )));
}

#[tokio::test]
async fn messaging_tools_return_whole_messages() {
    let (big1, big2) = ("a".repeat(12_000), "b".repeat(12_000));
    let (answer, partial) = ("z".repeat(20_000), "y".repeat(20_000));
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    calls(vec![
                        (
                            "spawn_agent",
                            json!({"task": "chatty task", "name": "chatty"}),
                        ),
                        ("spawn_agent", json!({"task": "slow task", "name": "slow"})),
                    ]),
                    call("receive", json!({"yield_after": 30})),
                    call("wait", json!({"agents": ["chatty"]})),
                    call("cancel_agent", json!({"to": "slow"})),
                    text("done"),
                ],
            ),
            rule(
                "chatty task",
                1,
                vec![
                    calls(vec![
                        ("send_message", json!({"to": "parent", "body": big1})),
                        ("send_message", json!({"to": "parent", "body": big2})),
                    ]),
                    text(&answer),
                ],
            ),
            rule(
                "slow task",
                1,
                vec![ModelResponse {
                    content: vec![
                        ContentBlock::Text {
                            text: partial.clone(),
                        },
                        ContentBlock::ToolUse {
                            id: "t0".into(),
                            name: "spawn_agent".into(),
                            input: json!({"task": "helper task"}),
                        },
                    ],
                    ..call("spawn_agent", json!({}))
                }],
            ),
        ],
        vec![
            (at("chatty task", 0), at("root task", 1)),
            (at("root task", 1), at("chatty task", 1)),
            (at("chatty task", 1), at("root task", 2)),
            (at("root task", 3), at("slow task", 1)),
            (at("slow task", 1), never()),
            (at("helper task", 0), never()),
        ],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits::default(),
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 5));
    // Each result is longer than the 20,000 character tool output cap.
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![(
            format!(
                "[message from agent 1 (chatty)]\n{big1}\n\n[message from agent 1 (chatty)]\n{big2}"
            ),
            false
        )]
    );
    assert_eq!(
        results(&last(&provider.request("root task", 3))),
        vec![(
            format!("[result from agent 1 (chatty): completed]\n{answer}"),
            false
        )]
    );
    assert_eq!(
        results(&last(&provider.request("root task", 4))),
        vec![(
            format!("[cancelled from agent 2 (slow): cancelled]\n{partial}"),
            false
        )]
    );
}

#[tokio::test]
async fn only_a_valid_result_finishes_a_contract_child() {
    // An inherited tool tries to finish the child with answers the contract refuses.
    let python = tool(|input, _| async move {
        let mut output = ToolOutput::text("ok");
        output.final_answer = Some(match input["answer"].as_str().unwrap() {
            "text" => Answer::Text("not json".into()),
            "mismatch" => {
                Answer::Value(serde_json::value::to_raw_value(&json!({"dates": "2026"})).unwrap())
            }
            _ => Answer::Value(
                serde_json::value::to_raw_value(&json!({"dates": ["x".repeat(300)]})).unwrap(),
            ),
        });
        output
    });
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "extract task", "name": "extract", "output": dates_schema()}),
                    ),
                    call("wait", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "extract task",
                1,
                vec![
                    call("python", json!({"answer": "text"})),
                    call("python", json!({"answer": "mismatch"})),
                    call("python", json!({"answer": "big"})),
                    call("submit_result", json!({"dates": ["2026-10-07"]})),
                ],
            ),
        ],
        vec![(at("extract task", 0), at("root task", 1))],
    );
    let runtime = setup(
        provider.clone(),
        python,
        Limits {
            message_chars: 200,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    for (turn, reason) in [
        (
            1,
            "a task with an output schema finishes only through submit_result",
        ),
        (
            2,
            "result does not match the output schema: invalid input type, expected \"array\"",
        ),
        (3, "result exceeds 200 characters"),
    ] {
        assert_eq!(
            results(&last(&provider.request("extract task", turn))),
            vec![(format!("ok\nfinal answer not accepted: {reason}"), true)]
        );
    }
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![(
            "[result from agent 1 (extract): completed]\n{\"dates\":[\"2026-10-07\"]}".into(),
            false
        )]
    );
}

#[tokio::test]
async fn dropped_send_is_still_recorded_when_its_recipient_ends() {
    let (trace, acks) = Acks::new(
        |event| matches!(event, TraceEvent::MessageSent { message } if message.body == "late"),
    );
    let rx = trace.subscribe();
    let woken = Arc::new(tokio::sync::Notify::new());
    let (wake, gate) = (woken.clone(), acks.clone());
    let python = tool(move |_, cx| {
        let (woken, acks) = (wake.clone(), gate.clone());
        async move {
            if cx.node.depth == 0 {
                cx.node
                    .spawn_agent(ChildSpec::new("child task"), Owner::Node)
                    .unwrap();
                tokio::time::timeout(LIMIT, woken.notified()).await.unwrap();
                let mut result = ToolOutput::text("ok");
                result.final_answer = Some(Answer::Text("final".into()));
                return result;
            }
            let parent = cx.node.resolve("parent").unwrap();
            {
                let mut send = pin!(cx.node.send(parent, "late"));
                assert!(futures::poll!(send.as_mut()).is_pending());
                woken.notify_one();
                // The parent closes its mailbox, then cancels this child.
                tokio::time::timeout(LIMIT, cx.cancel.cancelled())
                    .await
                    .unwrap();
                acks.release();
            }
            ToolOutput::text("dropped")
        }
    });
    let provider = Gated::new(
        vec![
            rule("root task", 0, vec![call("python", json!({}))]),
            rule("child task", 1, vec![call("python", json!({}))]),
        ],
        vec![],
    );
    let runtime = setup(provider, python, Limits::default(), trace.clone());
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    trace.finish().await.unwrap();
    let live = records(rx);
    assert!(
        undelivered(&live)
            .iter()
            .any(|message| message.body == "late")
    );
    assert_settled(&live);
}

#[tokio::test]
async fn receive_hands_over_messages_without_waiting_for_their_record() {
    let (trace, acks) = Acks::new(|event| {
        matches!(
            event,
            TraceEvent::MessageDelivered {
                via: Delivery::Receive,
                ..
            }
        )
    });
    let rx = trace.subscribe();
    let returned = Arc::new(Mutex::new(None));
    let (seen, gate) = (returned.clone(), acks.clone());
    let python = tool(move |_, cx| {
        let (seen, acks) = (seen.clone(), gate.clone());
        async move {
            if cx.node.depth == 1 {
                let parent = cx.node.resolve("parent").unwrap();
                cx.node.send(parent, "first").await.unwrap();
                return ToolOutput::text("sent");
            }
            cx.node
                .spawn_agent(ChildSpec::new("child task"), Owner::Node)
                .unwrap();
            eventually(&cx, "the message", || cx.node.pending_messages() == 1).await;
            // A caller that gives up on a receive must not lose what it took.
            let got = {
                let mut receive = pin!(cx.node.receive(Duration::ZERO));
                match futures::poll!(receive.as_mut()) {
                    Poll::Ready(result) => result.unwrap().len(),
                    Poll::Pending => 0,
                }
            };
            *seen.lock().unwrap() = Some(got);
            acks.release();
            let mut result = ToolOutput::text("ok");
            result.final_answer = Some(Answer::Text("final".into()));
            result
        }
    });
    let provider = Gated::new(
        vec![
            rule("root task", 0, vec![call("python", json!({}))]),
            rule("child task", 1, vec![call("python", json!({}))]),
        ],
        vec![(at("child task", 1), never())],
    );
    let runtime = setup(provider, python, Limits::default(), trace.clone());
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    trace.finish().await.unwrap();
    assert_eq!(*returned.lock().unwrap(), Some(1));
    assert_settled(&records(rx));
}

#[tokio::test]
async fn a_held_receive_cannot_overfill_the_mailbox() {
    let (trace, acks) = Acks::new(|event| {
        matches!(
            event,
            TraceEvent::MessageDelivered {
                via: Delivery::Receive,
                ..
            }
        )
    });
    let (to_child, to_root) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let observed = Arc::new(Mutex::new(None));
    let (child_bell, root_bell, seen, gate) = (
        to_child.clone(),
        to_root.clone(),
        observed.clone(),
        acks.clone(),
    );
    let python = tool(move |_, cx| {
        let (to_child, to_root, seen, acks) = (
            child_bell.clone(),
            root_bell.clone(),
            seen.clone(),
            gate.clone(),
        );
        async move {
            if cx.node.depth == 1 {
                let parent = cx.node.resolve("parent").unwrap();
                cx.node.send(parent, "A").await.unwrap();
                tokio::time::timeout(LIMIT, to_child.notified())
                    .await
                    .unwrap();
                cx.node.send(parent, "B").await.unwrap();
                assert!(matches!(
                    cx.node.send(parent, "C").await,
                    Err(RecursionError::MailboxFull { agent: 0 })
                ));
                to_root.notify_one();
                return ToolOutput::text("sent");
            }
            cx.node
                .spawn_agent(ChildSpec::new("child task"), Owner::Node)
                .unwrap();
            eventually(&cx, "A", || cx.node.pending_messages() == 1).await;
            let got = {
                let mut receive = pin!(cx.node.receive(Duration::ZERO));
                let got = match futures::poll!(receive.as_mut()) {
                    Poll::Ready(result) => result.unwrap().len(),
                    Poll::Pending => 0,
                };
                to_child.notify_one();
                tokio::time::timeout(LIMIT, to_root.notified())
                    .await
                    .unwrap();
                got
            };
            *seen.lock().unwrap() = Some((got, cx.node.pending_messages()));
            acks.release();
            let mut result = ToolOutput::text("ok");
            result.final_answer = Some(Answer::Text("final".into()));
            result
        }
    });
    let provider = Gated::new(
        vec![
            rule("root task", 0, vec![call("python", json!({}))]),
            rule("child task", 1, vec![call("python", json!({}))]),
        ],
        vec![(at("child task", 1), never())],
    );
    let runtime = setup(
        provider,
        python,
        Limits {
            mailbox_capacity: 1,
            ..Limits::default()
        },
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    // A was handed over, B took its place, and the mailbox never held two.
    assert_eq!(*observed.lock().unwrap(), Some((1, 1)));
}

#[tokio::test]
async fn an_accepted_result_survives_a_later_cancel() {
    let (trace, acks) = Acks::new(|event| {
        matches!(
            event,
            TraceEvent::ToolResult { node: 1, content, .. } if content == "result submitted"
        )
    });
    let ended = Arc::new(Mutex::new(None));
    let (seen, gate) = (ended.clone(), acks.clone());
    let python = tool(move |_, cx| {
        let (seen, acks) = (seen.clone(), gate.clone());
        async move {
            let child = cx
                .node
                .spawn_agent(
                    ChildSpec {
                        name: Some("extract".into()),
                        output: Some(dates_schema()),
                        ..ChildSpec::new("extract task")
                    },
                    Owner::Node,
                )
                .unwrap();
            // The submission is accepted; its tool result is still being recorded.
            acks.held(1).await;
            child.cancel();
            acks.release();
            let outcome = tokio::time::timeout(LIMIT, child.result()).await.unwrap();
            *seen.lock().unwrap() = Some((outcome.status, outcome.answer.text()));
            let mut result = ToolOutput::text("ok");
            result.final_answer = Some(Answer::Text("final".into()));
            result
        }
    });
    let provider = Gated::new(
        vec![
            rule("root task", 0, vec![call("python", json!({}))]),
            rule(
                "extract task",
                1,
                vec![call("submit_result", json!({"dates": ["2026-10-07"]}))],
            ),
        ],
        vec![],
    );
    let runtime = setup(provider, python, Limits::default(), trace);
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(
        *ended.lock().unwrap(),
        Some((Status::Completed, "{\"dates\":[\"2026-10-07\"]}".into()))
    );
}

#[tokio::test]
async fn wait_returns_a_childs_progress_before_its_result() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "chatty task", "name": "chatty"}),
                    ),
                    call("wait", json!({})),
                    text("done"),
                ],
            ),
            rule(
                "chatty task",
                1,
                vec![
                    call("send_message", json!({"to": "parent", "body": "progress"})),
                    text("final"),
                ],
            ),
        ],
        vec![
            // The progress message is queued behind the parent's turn boundary.
            (at("chatty task", 0), at("root task", 1)),
            (at("root task", 1), at("chatty task", 1)),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(provider.clone(), idle_tool(), Limits::default(), trace);
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    let waited = last(&provider.request("root task", 2));
    assert_eq!(
        results(&waited),
        vec![(
            "[message from agent 1 (chatty)]\nprogress\n\n[result from agent 1 (chatty): completed]\nfinal"
                .into(),
            false
        )]
    );
    assert!(texts(&waited).is_empty());
    let live = records(rx);
    let ids = sent(&live).iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(delivered(&live), vec![(0, ids, Delivery::Wait)]);
}

#[tokio::test]
async fn cancel_agent_returns_a_childs_progress_before_its_notice() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call("spawn_agent", json!({"task": "slow task", "name": "slow"})),
                    call("cancel_agent", json!({"to": "slow"})),
                    text("done"),
                ],
            ),
            rule(
                "slow task",
                1,
                vec![call(
                    "send_message",
                    json!({"to": "parent", "body": "progress"}),
                )],
            ),
        ],
        vec![
            (at("slow task", 0), at("root task", 1)),
            (at("root task", 1), at("slow task", 1)),
            (at("slow task", 1), never()),
        ],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits::default(),
        TraceSink::ephemeral(),
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    let cancelled = last(&provider.request("root task", 2));
    assert_eq!(
        results(&cancelled),
        vec![(
            "[message from agent 1 (slow)]\nprogress\n\n[cancelled from agent 1 (slow): cancelled]"
                .into(),
            false
        )]
    );
    assert!(texts(&cancelled).is_empty());
}

/// Each of these messages renders to 72 characters.
fn progress(digit: char) -> String {
    digit.to_string().repeat(40)
}

#[tokio::test]
async fn turn_deliveries_page_by_budget_across_turns() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "worker task", "name": "worker"}),
                    ),
                    text("waiting"),
                    text("still waiting"),
                    call("cancel_agent", json!({"to": "worker"})),
                    text("done"),
                ],
            ),
            rule(
                "worker task",
                1,
                vec![calls(vec![
                    (
                        "send_message",
                        json!({"to": "parent", "body": progress('1')}),
                    ),
                    (
                        "send_message",
                        json!({"to": "parent", "body": progress('2')}),
                    ),
                    (
                        "send_message",
                        json!({"to": "parent", "body": progress('3')}),
                    ),
                ])],
            ),
        ],
        vec![
            // All three messages are queued before the parent's first idle delivery.
            (at("worker task", 0), at("root task", 1)),
            (at("root task", 1), at("worker task", 1)),
            (at("worker task", 1), never()),
        ],
    );
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits {
            delivery_chars: 150,
            ..Limits::default()
        },
        trace,
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 5));
    let line = |digit| format!("[message from agent 1 (worker)]\n{}", progress(digit));
    // Two fit the budget; the idle parent does not end while the third waits.
    assert_eq!(
        texts(&last(&provider.request("root task", 2))),
        vec![
            line('1'),
            line('2'),
            "[1 more message waiting; it follows at your next turn]".into(),
        ]
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec![line('3')]
    );
    let live = records(rx);
    let ids = sent(&live).iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(
        delivered(&live),
        vec![
            (0, ids[..2].to_vec(), Delivery::Turn),
            (0, vec![ids[2]], Delivery::Turn),
            (0, vec![ids[3]], Delivery::Cancel),
        ]
    );
}

#[tokio::test]
async fn an_oversized_message_still_goes_through_and_the_rest_follows() {
    let big = "b".repeat(500);
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "worker task", "name": "worker"}),
                    ),
                    call("receive", json!({"yield_after": 30})),
                    call("cancel_agent", json!({"to": "worker"})),
                    text("done"),
                ],
            ),
            rule(
                "worker task",
                1,
                vec![calls(vec![
                    ("send_message", json!({"to": "parent", "body": big})),
                    ("send_message", json!({"to": "parent", "body": "small one"})),
                    ("send_message", json!({"to": "parent", "body": "small two"})),
                ])],
            ),
        ],
        vec![
            (at("worker task", 0), at("root task", 1)),
            (at("root task", 1), at("worker task", 1)),
            (at("worker task", 1), never()),
        ],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits {
            delivery_chars: 100,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    // The first message exceeds the budget alone and is still handed over whole;
    // the rest follows at the same boundary, in order, within its own budget.
    let boundary = last(&provider.request("root task", 2));
    assert_eq!(
        results(&boundary),
        vec![(
            format!(
                "[message from agent 1 (worker)]\n{big}\n\n2 more messages waiting; they follow at your next turn"
            ),
            false
        )]
    );
    assert_eq!(
        texts(&boundary),
        vec![
            "[message from agent 1 (worker)]\nsmall one",
            "[message from agent 1 (worker)]\nsmall two",
        ]
    );
}

#[tokio::test]
async fn wait_defers_a_result_behind_progress_that_does_not_fit() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "chatty task", "name": "chatty"}),
                    ),
                    call("wait", json!({})),
                    text("reading"),
                    text("done"),
                ],
            ),
            rule(
                "chatty task",
                1,
                vec![
                    calls(vec![
                        (
                            "send_message",
                            json!({"to": "parent", "body": progress('1')}),
                        ),
                        (
                            "send_message",
                            json!({"to": "parent", "body": progress('2')}),
                        ),
                    ]),
                    text("final"),
                ],
            ),
        ],
        vec![
            (at("chatty task", 0), at("root task", 1)),
            (at("root task", 1), at("chatty task", 1)),
        ],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits {
            delivery_chars: 100,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    let line = |digit| format!("[message from agent 1 (chatty)]\n{}", progress(digit));
    // The result never overtakes the progress: one message per page, then the result.
    let first = last(&provider.request("root task", 2));
    assert_eq!(
        results(&first),
        vec![(
            format!(
                "{}\n\nagent 1 finished, but 2 of its messages, its result last, did not fit; they follow at your next turn",
                line('1')
            ),
            false
        )]
    );
    assert_eq!(
        texts(&first),
        vec![
            line('2'),
            "[1 more message waiting; it follows at your next turn]".into(),
        ]
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec!["[result from agent 1 (chatty): completed]\nfinal"]
    );
}

#[tokio::test]
async fn cancel_agent_defers_a_notice_that_does_not_fit() {
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call("spawn_agent", json!({"task": "slow task", "name": "slow"})),
                    call("cancel_agent", json!({"to": "slow"})),
                    text("reading"),
                    text("done"),
                ],
            ),
            rule(
                "slow task",
                1,
                vec![calls(vec![
                    (
                        "send_message",
                        json!({"to": "parent", "body": progress('1')}),
                    ),
                    (
                        "send_message",
                        json!({"to": "parent", "body": progress('2')}),
                    ),
                ])],
            ),
        ],
        vec![
            (at("slow task", 0), at("root task", 1)),
            (at("root task", 1), at("slow task", 1)),
            (at("slow task", 1), never()),
        ],
    );
    let runtime = setup(
        provider.clone(),
        idle_tool(),
        Limits {
            delivery_chars: 100,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!((outcome.status, outcome.turns), (Status::Completed, 4));
    let line = |digit| format!("[message from agent 1 (slow)]\n{}", progress(digit));
    let first = last(&provider.request("root task", 2));
    assert_eq!(
        results(&first),
        vec![(
            format!(
                "{}\n\nagent 1 finished, but 2 of its messages, its notice last, did not fit; they follow at your next turn",
                line('1')
            ),
            false
        )]
    );
    assert_eq!(
        texts(&first),
        vec![
            line('2'),
            "[1 more message waiting; it follows at your next turn]".into(),
        ]
    );
    assert_eq!(
        texts(&last(&provider.request("root task", 3))),
        vec!["[cancelled from agent 1 (slow): cancelled]"]
    );
}

#[tokio::test]
async fn a_senders_messages_are_queued_in_send_order() {
    let (trace, acks) = Acks::new(
        |event| matches!(event, TraceEvent::MessageSent { message } if message.body == "A"),
    );
    let order = Arc::new(Mutex::new(None));
    let (seen, gate) = (order.clone(), acks.clone());
    let python = tool(move |_, cx| {
        let (seen, acks) = (seen.clone(), gate.clone());
        async move {
            if cx.node.depth == 1 {
                let parent = cx.node.resolve("parent").unwrap();
                {
                    // A is accepted and its record held; its waiter is dropped.
                    let mut a = pin!(cx.node.send(parent, "A"));
                    assert!(futures::poll!(a.as_mut()).is_pending());
                }
                let mut b = pin!(cx.node.send(parent, "B"));
                // Give B every chance to overtake A while A's record is held.
                let early = tokio::time::timeout(Duration::from_millis(100), b.as_mut()).await;
                acks.release();
                match early {
                    Ok(sent) => sent.unwrap(),
                    Err(_) => tokio::time::timeout(LIMIT, b).await.unwrap().unwrap(),
                };
                return ToolOutput::text("sent");
            }
            cx.node
                .spawn_agent(ChildSpec::new("child task"), Owner::Node)
                .unwrap();
            eventually(&cx, "both messages", || cx.node.pending_messages() == 2).await;
            let got = cx.node.receive(Duration::ZERO).await.unwrap();
            *seen.lock().unwrap() = Some(got.iter().map(|m| m.body.clone()).collect::<Vec<_>>());
            let mut result = ToolOutput::text("ok");
            result.final_answer = Some(Answer::Text("final".into()));
            result
        }
    });
    let provider = Gated::new(
        vec![
            rule("root task", 0, vec![call("python", json!({}))]),
            rule("child task", 1, vec![call("python", json!({}))]),
        ],
        vec![(at("child task", 1), never())],
    );
    let runtime = setup(provider, python, Limits::default(), trace);
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(
        *order.lock().unwrap(),
        Some(vec!["A".to_string(), "B".to_string()])
    );
}

#[tokio::test]
async fn a_failed_message_record_fails_the_run() {
    // The store refuses delivery records and accepts everything else.
    let trace = TraceSink::with_store(|record: TraceRecord| {
        let refused = matches!(record.event, TraceEvent::MessageDelivered { .. });
        async move {
            if refused {
                anyhow::bail!("store unavailable");
            }
            Ok(())
        }
    });
    let provider = Gated::new(
        vec![
            rule(
                "root task",
                0,
                vec![
                    call(
                        "spawn_agent",
                        json!({"task": "child task", "name": "worker"}),
                    ),
                    text("waiting"),
                    text("done"),
                ],
            ),
            rule("child task", 1, vec![text("child answer")]),
        ],
        vec![(at("child task", 0), at("root task", 1))],
    );
    let runtime = setup(provider, idle_tool(), Limits::default(), trace.clone());
    assert!(runtime.run(spec()).await.is_err());
    assert!(trace.finish().await.is_err());
}
