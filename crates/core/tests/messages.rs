use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use kyora_core::{
    AgentSpec, Answer, ChildSpec, Delivery, Effect, Envelope, Limits, MessageId, MessageKind,
    NodeCtx, NodeId, Owner, RecursionError, Runtime, RuntimeConfig, Status, Tool, ToolCx,
    ToolOutput, Toolset, agent_tools,
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
    sync::{Arc, Mutex},
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

type Pred = Box<dyn Fn(&ModelRequest) -> bool + Send + Sync>;
/// Scripted responses that record every request. A gated request is held until
/// a request matching its release predicate has been seen, or it is cancelled.
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
        loop {
            let found = seen.borrow_and_update().iter().any(&pred);
            if found {
                return;
            }
            seen.changed().await.unwrap();
        }
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
            loop {
                let released = seen.borrow_and_update().iter().any(release);
                if released {
                    break;
                }
                tokio::select! {
                    _ = seen.changed() => {}
                    _ = cancel.cancelled() => return Err(ProviderError::cancelled(false)),
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
fn status(records: &[TraceRecord], node: NodeId) -> Status {
    records
        .iter()
        .find_map(|record| match &record.event {
            TraceEvent::NodeEnd { outcome } if outcome.node == node => Some(outcome.status),
            _ => None,
        })
        .expect("node ended")
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
        while cx.node.pending_messages() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
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
            while cx.node.pending_messages() < 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
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
    assert_eq!(
        results(&last(&provider.request("root task", 2))),
        vec![(
            "[result from agent 1 (a): completed]\na done\n\n[result from agent 2 (b): completed]\nb done"
                .into(),
            false
        )]
    );
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
