use async_trait::async_trait;
use futures::{StreamExt, stream};
use kyora_core::{
    AgentSpec, Answer, Effect, Limits, LlmCall, Runtime, RuntimeConfig, Status, Tool, ToolCx,
    ToolOutput, Toolset, TraceEvent, TraceSink, trace::TraceRecord,
};
use kyora_protocol::{
    BlockStart, ContentBlock, Message, ModelInfo, ModelRequest, ModelResponse, StopReason,
    StreamEvent, ToolSpec, Usage,
};
use kyora_providers::{EventStream, ModelProvider, ProviderError, RetryPolicy, fake::FnProvider};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn response(content: Vec<ContentBlock>, stop: StopReason) -> ModelResponse {
    ModelResponse {
        id: None,
        model: String::new(),
        content,
        stop_reason: stop,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            ..Usage::default()
        },
        usage_iterations: vec![],
    }
}
fn text(value: &str) -> ContentBlock {
    ContentBlock::Text { text: value.into() }
}
fn call(id: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: "test".into(),
        input: json!({"text":id}),
    }
}
fn runtime(
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    limits: Limits,
) -> (Runtime, tokio::sync::broadcast::Receiver<TraceRecord>) {
    let trace = TraceSink::ephemeral();
    let receiver = trace.subscribe();
    let runtime = Runtime::new(RuntimeConfig {
        providers: BTreeMap::from([("fake".into(), provider)]),
        toolsets: Arc::new(Toolset::new(tools).unwrap()),
        limits,
        retry: RetryPolicy {
            base: Duration::ZERO,
            ..RetryPolicy::default()
        },
        llm_model: "fake/leaf".parse().unwrap(),
        trace,
        session: "test".into(),
    })
    .unwrap();
    (runtime, receiver)
}
fn spec() -> AgentSpec {
    let mut spec = AgentSpec::new("task", std::env::current_dir().unwrap());
    spec.model = "fake/root".parse().unwrap();
    spec
}
fn drain(mut receiver: tokio::sync::broadcast::Receiver<TraceRecord>) -> Vec<TraceEvent> {
    let mut events = vec![];
    while let Ok(record) = receiver.try_recv() {
        events.push(record.event);
    }
    events
}
fn history(events: &[TraceEvent]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}
struct TestTool {
    seen: Arc<Mutex<Vec<String>>>,
    final_answer: bool,
}
#[async_trait]
impl Tool for TestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> ToolOutput {
        self.seen
            .lock()
            .unwrap()
            .push(input["text"].as_str().unwrap().into());
        ToolOutput {
            content: vec![kyora_protocol::ToolResultPart::Text { text: "ok".into() }],
            is_error: false,
            final_answer: self.final_answer.then(|| Answer::Text("committed".into())),
        }
    }
}
#[tokio::test]
async fn sequential_round_trip_freezes_prefix_and_records_one_result_per_call() {
    let seen = Arc::new(Mutex::new(vec![]));
    let tool = Arc::new(TestTool {
        seen: seen.clone(),
        final_answer: false,
    });
    let requests = Arc::new(Mutex::new(vec![]));
    let saved = requests.clone();
    let provider = FnProvider::new(move |request: &ModelRequest| {
        saved.lock().unwrap().push(request.clone());
        if request.messages.len() == 1 {
            Ok(response(vec![call("a"), call("b")], StopReason::ToolUse))
        } else {
            Ok(response(vec![text("done")], StopReason::EndTurn))
        }
    });
    let (rt, rx) = runtime(Arc::new(provider), vec![tool], Limits::default());
    let outcome = rt.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.answer.text(), "done");
    assert_eq!(*seen.lock().unwrap(), vec!["a", "b"]);
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].tools, requests[1].tools);
    let events = drain(rx);
    let h = history(&events);
    assert_eq!(h.len(), 4);
    assert_eq!(h[2].content.len(), 2);
    for (id, block) in ["a", "b"].iter().zip(&h[2].content) {
        assert!(
            matches!(block,ContentBlock::ToolResult {tool_use_id,is_error:false,..} if tool_use_id==id)
        );
    }
    assert_eq!(outcome.usage_self.total(), 240);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
}
#[tokio::test]
async fn first_final_answer_skips_remaining_tools() {
    let seen = Arc::new(Mutex::new(vec![]));
    let tool = Arc::new(TestTool {
        seen: seen.clone(),
        final_answer: true,
    });
    let provider =
        FnProvider::new(|_| Ok(response(vec![call("a"), call("b")], StopReason::ToolUse)));
    let (rt, rx) = runtime(Arc::new(provider), vec![tool], Limits::default());
    let result = rt.run(spec()).await.unwrap();
    assert_eq!(result.answer.text(), "committed");
    assert_eq!(*seen.lock().unwrap(), vec!["a"]);
    let h = history(&drain(rx));
    assert!(matches!(
        h[2].content[1],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
}
#[tokio::test]
async fn all_stop_reasons_pair_pending_calls_without_execution() {
    for (reason, status) in [
        (StopReason::Refusal, Status::Refused),
        (
            StopReason::ModelContextWindowExceeded,
            Status::ContextExhausted,
        ),
        (StopReason::EndTurn, Status::Completed),
        (StopReason::Other("unknown".into()), Status::Failed),
        (StopReason::MaxTokens, Status::MaxTurns),
        (StopReason::PauseTurn, Status::MaxTurns),
    ] {
        let seen = Arc::new(Mutex::new(vec![]));
        let tool = Arc::new(TestTool {
            seen: seen.clone(),
            final_answer: false,
        });
        let provider = FnProvider::new(move |_| Ok(response(vec![call("a")], reason.clone())));
        let (rt, rx) = runtime(
            Arc::new(provider),
            vec![tool],
            Limits {
                max_turns: 1,
                ..Limits::default()
            },
        );
        assert_eq!(rt.run(spec()).await.unwrap().status, status);
        assert!(seen.lock().unwrap().is_empty());
        let h = history(&drain(rx));
        assert_eq!(h.len(), 3);
        assert!(matches!(
            h[2].content[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
    }
}
#[tokio::test]
async fn max_tokens_without_tools_appends_continue_pause_turn_replays_unchanged() {
    for reason in [StopReason::MaxTokens, StopReason::PauseTurn] {
        let wanted = reason.clone();
        let provider = FnProvider::new(move |req: &ModelRequest| {
            if req.messages.len() == 1 {
                Ok(response(vec![text("partial")], wanted.clone()))
            } else {
                if wanted == StopReason::MaxTokens {
                    assert_eq!(req.messages.last().unwrap().text(), "continue");
                } else {
                    assert_eq!(req.messages.len(), 2);
                }
                Ok(response(vec![text("done")], StopReason::EndTurn))
            }
        });
        let (rt, _) = runtime(Arc::new(provider), vec![], Limits::default());
        assert_eq!(rt.run(spec()).await.unwrap().answer.text(), "done");
    }
}
struct StreamProvider<F>(F);
#[async_trait]
impl<F> ModelProvider for StreamProvider<F>
where
    F: Fn(ModelRequest) -> Result<EventStream, ProviderError> + Send + Sync,
{
    fn name(&self) -> &str {
        "fake"
    }
    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError> {
        Ok(ModelInfo {
            id: model.into(),
            max_output_tokens: Some(64_000),
            context_window: Some(100_000),
        })
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        (self.0)(request)
    }
}
fn raw_events(raw: &str, stop: StopReason) -> Vec<Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::MessageStart {
            id: None,
            model: "test".into(),
            usage: Usage {
                input_tokens: 100,
                ..Usage::default()
            },
        }),
        Ok(StreamEvent::BlockStart {
            index: 4,
            block: BlockStart::ToolUse {
                id: "a".into(),
                name: "test".into(),
            },
        }),
        Ok(StreamEvent::ToolInputDelta {
            index: 4,
            partial_json: raw.into(),
        }),
        Ok(StreamEvent::BlockStop { index: 4 }),
        Ok(StreamEvent::MessageDelta {
            stop_reason: Some(stop),
            usage: Usage {
                output_tokens: 10,
                ..Usage::default()
            },
        }),
        Ok(StreamEvent::MessageStop),
    ]
}
#[tokio::test]
async fn malformed_max_tokens_retries_once_before_admission_and_preserves_raw() {
    let caps = Arc::new(Mutex::new(vec![]));
    let saved = caps.clone();
    let provider = StreamProvider(move |req: ModelRequest| {
        saved.lock().unwrap().push(req.max_tokens);
        Ok(Box::pin(stream::iter(raw_events(
            "{\"text\":",
            StopReason::MaxTokens,
        ))) as EventStream)
    });
    let seen = Arc::new(Mutex::new(vec![]));
    let tool = Arc::new(TestTool {
        seen: seen.clone(),
        final_answer: false,
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![tool],
        Limits {
            max_turns: 1,
            ..Limits::default()
        },
    );
    let outcome = rt.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::MaxTurns);
    assert_eq!(outcome.turns, 1);
    assert_eq!(*caps.lock().unwrap(), vec![32_000, 64_000]);
    assert!(seen.lock().unwrap().is_empty());
    let events = drain(rx);
    let h = history(&events);
    assert_eq!(h.len(), 3);
    assert_eq!(h[1].tool_uses().next().unwrap().2, &json!({}));
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &h[2].content[0]
    else {
        panic!("result missing")
    };
    assert!(*is_error);
    let kyora_protocol::ToolResultPart::Text { text } = &content[0];
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap()["INVALID_JSON"],
        "{\"text\":"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TraceEvent::StreamReset { .. }))
    );
    assert_eq!(outcome.usage_self.total(), 220);
}
#[tokio::test]
async fn malformed_context_exhaustion_and_unknown_tool_get_results() {
    let provider = StreamProvider(|_| {
        Ok(Box::pin(stream::iter(raw_events(
            "{",
            StopReason::ModelContextWindowExceeded,
        ))) as EventStream)
    });
    let (rt, rx) = runtime(Arc::new(provider), vec![], Limits::default());
    assert_eq!(
        rt.run(spec()).await.unwrap().status,
        Status::ContextExhausted
    );
    assert_eq!(history(&drain(rx)).len(), 3);
    let provider = FnProvider::new(|_| Ok(response(vec![call("a")], StopReason::ToolUse)));
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![],
        Limits {
            max_turns: 1,
            ..Limits::default()
        },
    );
    rt.run(spec()).await.unwrap();
    assert!(matches!(
        history(&drain(rx))[2].content[0],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
}
#[tokio::test]
async fn retries_charge_each_attempt_and_reset_partial_streams() {
    let count = Arc::new(AtomicUsize::new(0));
    let saved = count.clone();
    let provider = StreamProvider(move |_| {
        if saved.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut events = raw_events("{}", StopReason::ToolUse);
            events.truncate(3);
            events.push(Err(ProviderError::Transport("reset".into())));
            Ok(Box::pin(stream::iter(events)) as EventStream)
        } else {
            Ok(Box::pin(stream::iter(raw_events("{}", StopReason::Refusal))) as EventStream)
        }
    });
    let (rt, rx) = runtime(Arc::new(provider), vec![], Limits::default());
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Refused);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let events = drain(rx);
    let starts = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::AttemptStart { reserved, .. } => Some(*reserved),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ends = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::AttemptEnd { charged, .. } => Some(*charged),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(ends, vec![starts[0], 110]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TraceEvent::StreamReset { .. }))
    );
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
}
#[tokio::test]
async fn cancellation_mid_stream_settles_and_total_timeout_retries_are_bounded() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    let provider = StreamProvider(move |_| {
        notify.notify_one();
        Ok(Box::pin(
            stream::iter(raw_events("{}", StopReason::ToolUse).into_iter().take(3))
                .chain(stream::pending()),
        ) as EventStream)
    });
    let (rt, rx) = runtime(Arc::new(provider), vec![], Limits::default());
    let active = rt.clone();
    let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
    entered.notified().await;
    rt.cancel();
    assert_eq!(task.await.unwrap().status, Status::Cancelled);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    let events = drain(rx);
    let reserved = events
        .iter()
        .find_map(|e| match e {
            TraceEvent::AttemptStart { reserved, .. } => Some(*reserved),
            _ => None,
        })
        .unwrap();
    assert_eq!(rt.ledger().snapshot(0).used, reserved);
    let provider = StreamProvider(|_| Ok(Box::pin(stream::pending()) as EventStream));
    let (rt, _) = runtime(
        Arc::new(provider),
        vec![],
        Limits {
            run_timeout: Duration::from_millis(15),
            ..Limits::default()
        },
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Timeout);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
}
struct WaitingTool {
    entered: Arc<tokio::sync::Notify>,
    leaf: bool,
}
#[async_trait]
impl Tool for WaitingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _input: Value, cx: ToolCx) -> ToolOutput {
        self.entered.notify_one();
        if self.leaf {
            let _ = cx.node.llm(LlmCall::new("leaf"), &cx.cancel).await;
        } else {
            cx.cancel.cancelled().await;
        }
        ToolOutput::error("cancelled")
    }
}
#[tokio::test]
async fn cancellation_mid_tool_pairs_all_calls() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let tool = WaitingTool {
        entered: entered.clone(),
        leaf: false,
    };
    let provider =
        FnProvider::new(|_| Ok(response(vec![call("a"), call("b")], StopReason::ToolUse)));
    let (rt, rx) = runtime(Arc::new(provider), vec![Arc::new(tool)], Limits::default());
    let active = rt.clone();
    let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
    entered.notified().await;
    rt.cancel();
    assert_eq!(task.await.unwrap().status, Status::Cancelled);
    let h = history(&drain(rx));
    assert_eq!(h[2].content.len(), 2);
    assert!(
        h[2].content
            .iter()
            .all(|c| matches!(c, ContentBlock::ToolResult { is_error: true, .. }))
    );
}
#[tokio::test]
async fn dropping_leaf_wait_on_tool_cancellation_still_settles_before_root_end() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let leaf_entered = Arc::new(tokio::sync::Notify::new());
    let notify = leaf_entered.clone();
    struct LeafProvider {
        notify: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ModelProvider for LeafProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn stream(
            &self,
            req: ModelRequest,
            cancel: CancellationToken,
        ) -> Result<EventStream, ProviderError> {
            if req.metadata.node_id.as_deref() == Some("0") {
                FnProvider::new(|_| Ok(response(vec![call("a")], StopReason::ToolUse)))
                    .stream(req, cancel)
                    .await
            } else {
                self.notify.notify_one();
                Ok(Box::pin(stream::pending()))
            }
        }
    }
    let (rt, rx) = runtime(
        Arc::new(LeafProvider { notify }),
        vec![Arc::new(WaitingTool {
            entered,
            leaf: true,
        })],
        Limits::default(),
    );
    let active = rt.clone();
    let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
    leaf_entered.notified().await;
    rt.cancel();
    let outcome = task.await.unwrap();
    assert_eq!(outcome.status, Status::Cancelled);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    assert!(outcome.usage_subtree.total() > outcome.usage_self.total());
    let events = drain(rx);
    let leaf_end = events
        .iter()
        .position(|e| matches!(e,TraceEvent::NodeEnd {outcome} if outcome.node==1))
        .unwrap();
    let root_end = events
        .iter()
        .position(|e| matches!(e,TraceEvent::NodeEnd {outcome} if outcome.node==0))
        .unwrap();
    assert!(leaf_end < root_end);
}

struct LeafTool;
struct CappedProvider<P>(P);
#[async_trait]
impl<P: ModelProvider> ModelProvider for CappedProvider<P> {
    fn name(&self) -> &str {
        self.0.name()
    }
    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError> {
        Ok(ModelInfo {
            id: model.into(),
            max_output_tokens: Some(128),
            ..ModelInfo::default()
        })
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        assert_eq!(request.max_tokens, 128);
        self.0.stream(request, cancel).await
    }
}

#[tokio::test]
async fn model_output_cap_applies_to_agents_leaves_and_truncated_input_retry() {
    let leaf_calls = Arc::new(AtomicUsize::new(0));
    let saved = leaf_calls.clone();
    let provider = FnProvider::new(move |req: &ModelRequest| {
        if req.metadata.node_id.as_deref() == Some("1") {
            saved.fetch_add(1, Ordering::SeqCst);
            Ok(response(vec![text("leaf")], StopReason::EndTurn))
        } else if req.messages.len() == 1 {
            Ok(response(vec![call("a")], StopReason::ToolUse))
        } else {
            Ok(response(vec![text("done")], StopReason::EndTurn))
        }
    });
    let (rt, _) = runtime(
        Arc::new(CappedProvider(provider)),
        vec![Arc::new(LeafTool)],
        Limits::default(),
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(leaf_calls.load(Ordering::SeqCst), 1);

    let calls = Arc::new(AtomicUsize::new(0));
    let saved = calls.clone();
    let provider = StreamProvider(move |_: ModelRequest| {
        saved.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(raw_events(
            "{\"text\":",
            StopReason::MaxTokens,
        ))) as EventStream)
    });
    let (rt, _) = runtime(
        Arc::new(CappedProvider(provider)),
        vec![],
        Limits {
            max_turns: 1,
            ..Limits::default()
        },
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::MaxTurns);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[async_trait]
impl Tool for LeafTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _input: Value, cx: ToolCx) -> ToolOutput {
        let mut call = LlmCall::new("leaf prompt");
        call.max_tokens = Some(512);
        match cx.node.llm(call, &cx.cancel).await {
            Ok(result) => ToolOutput::text(result.text),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}
#[tokio::test]
async fn leaf_completion_has_no_tools_uses_default_model_and_charges_subtree() {
    let provider = FnProvider::new(|req: &ModelRequest| {
        if req.metadata.node_id.as_deref() == Some("1") {
            assert_eq!(req.model, "leaf");
            assert!(req.tools.is_empty());
            assert_eq!(req.max_tokens, 512);
            assert_eq!(req.metadata.depth, 0);
            Ok(response(vec![text("leaf answer")], StopReason::EndTurn))
        } else if req.messages.len() == 1 {
            Ok(response(vec![call("a")], StopReason::ToolUse))
        } else {
            Ok(response(vec![text("done")], StopReason::EndTurn))
        }
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![Arc::new(LeafTool)],
        Limits {
            max_agents_total: 1,
            max_agents_live: 1,
            ..Limits::default()
        },
    );
    let result = rt.run(spec()).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.usage_self.total(), 240);
    assert_eq!(result.usage_subtree.total(), 360);
    let events = drain(rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e,TraceEvent::NodeStart {kind,parent:Some(0),..} if kind=="llm"))
    );
    assert!(events.iter().any(
        |e| matches!(e,TraceEvent::ToolResult {content,is_error:false,..} if content=="leaf answer")
    ));
}
struct BatchTool;
#[async_trait]
impl Tool for BatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _input: Value, cx: ToolCx) -> ToolOutput {
        let _ = tokio::join!(
            cx.node.llm(LlmCall::new("a"), &cx.cancel),
            cx.node.llm(LlmCall::new("b"), &cx.cancel)
        );
        ToolOutput::error("cancelled")
    }
}
#[tokio::test]
async fn waiting_for_model_slot_does_not_reserve_or_dispatch() {
    struct BatchProvider {
        entered: Arc<tokio::sync::Notify>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ModelProvider for BatchProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn stream(
            &self,
            req: ModelRequest,
            cancel: CancellationToken,
        ) -> Result<EventStream, ProviderError> {
            if req.metadata.node_id.as_deref() == Some("0") {
                FnProvider::new(|_| Ok(response(vec![call("a")], StopReason::ToolUse)))
                    .stream(req, cancel)
                    .await
            } else {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                Ok(Box::pin(stream::pending()))
            }
        }
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = BatchProvider {
        entered: entered.clone(),
        calls: calls.clone(),
    };
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![Arc::new(BatchTool)],
        Limits {
            max_inflight_requests: 1,
            ..Limits::default()
        },
    );
    let active = rt.clone();
    let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
    entered.notified().await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = drain(rx);
    let outstanding = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::AttemptStart { node, reserved, .. } if *node != 0 => Some(*reserved),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outstanding.len(), 1);
    assert_eq!(rt.ledger().snapshot(0).reserved, outstanding[0]);
    rt.cancel();
    task.await.unwrap();
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn idle_timeout_retries_with_fresh_charges_and_too_long_is_context_exhausted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let provider = StreamProvider(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::pending()) as EventStream)
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![],
        Limits {
            request_idle: Duration::from_millis(5),
            ..Limits::default()
        },
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    let events = drain(rx);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::AttemptEnd { .. }))
            .count(),
        5
    );
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    let provider = FnProvider::new(|_| Err(ProviderError::ContextTooLarge("input".into())));
    let (rt, _) = runtime(Arc::new(provider), vec![], Limits::default());
    assert_eq!(
        rt.run(spec()).await.unwrap().status,
        Status::ContextExhausted
    );
    assert_eq!(rt.ledger().snapshot(0).used, 0);
}

#[tokio::test]
async fn thinking_and_opaque_blocks_replay_verbatim() {
    let preserved = vec![
        ContentBlock::Thinking {
            thinking: "reason".into(),
            signature: Some("signed".into()),
        },
        ContentBlock::Opaque {
            provider: "fake".into(),
            kind: "redacted_thinking".into(),
            raw: json!({"data":"opaque"}),
        },
        call("a"),
    ];
    let saved = preserved.clone();
    let provider = FnProvider::new(move |req: &ModelRequest| {
        if req.messages.len() == 1 {
            Ok(response(saved.clone(), StopReason::ToolUse))
        } else {
            assert_eq!(req.messages[1].content, saved);
            Ok(response(vec![text("done")], StopReason::EndTurn))
        }
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![Arc::new(TestTool {
            seen: Arc::new(Mutex::new(vec![])),
            final_answer: false,
        })],
        Limits::default(),
    );
    rt.run(spec()).await.unwrap();
    assert_eq!(history(&drain(rx))[1].content, preserved);
}
#[tokio::test]
async fn not_sent_retry_is_zero_charged() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let provider = FnProvider::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(ProviderError::NotSent("connect".into()))
        } else {
            Ok(response(vec![text("done")], StopReason::EndTurn))
        }
    });
    let (rt, rx) = runtime(Arc::new(provider), vec![], Limits::default());
    let outcome = rt.run(spec()).await.unwrap();
    assert_eq!(outcome.usage_self.total(), 120);
    let events = drain(rx);
    let charges = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::AttemptEnd { charged, .. } => Some(*charged),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(charges, vec![0, 120]);
}
#[tokio::test]
async fn total_request_deadline_applies_even_when_deltas_keep_arriving() {
    let provider = StreamProvider(|_| {
        let prefix = vec![
            Ok(StreamEvent::MessageStart {
                id: None,
                model: "test".into(),
                usage: Usage::default(),
            }),
            Ok(StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            }),
        ];
        let deltas = stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(2)).await;
            Some((
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: "x".into(),
                }),
                (),
            ))
        });
        Ok(Box::pin(stream::iter(prefix).chain(deltas)) as EventStream)
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![],
        Limits {
            request_idle: Duration::from_secs(1),
            request_total: Duration::from_millis(20),
            ..Limits::default()
        },
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Failed);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    let events = drain(rx);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::AttemptEnd { .. }))
            .count(),
        5
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::StreamReset { .. }))
            .count(),
        4
    );
}

#[tokio::test]
async fn provider_dispatch_and_stream_panics_settle_root_reservations() {
    for polling in [false, true] {
        let provider = StreamProvider(move |_| {
            assert!(polling, "dispatch panic");
            Ok(Box::pin(stream::poll_fn(|_| panic!("stream panic"))) as EventStream)
        });
        let (rt, rx) = runtime(Arc::new(provider), vec![], Limits::default());
        assert!(rt.run(spec()).await.is_err());
        let snapshot = rt.ledger().snapshot(0);
        assert_eq!(snapshot.reserved, 0);
        assert!(snapshot.used > 0);
        assert!(drain(rx).iter().any(|event| matches!(
            event,
            TraceEvent::SessionEnd {
                status: Status::Failed
            }
        )));
    }
}

#[tokio::test]
async fn swallowed_leaf_panic_cannot_complete_root_or_leak_reservations() {
    struct LeafPanicTool;
    #[async_trait]
    impl Tool for LeafPanicTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "test".into(),
                input_schema: json!({"type":"object"}),
                ..ToolSpec::default()
            }
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        async fn call(&self, _: Value, cx: ToolCx) -> ToolOutput {
            assert!(
                cx.node
                    .llm(LlmCall::new("panic"), &cx.cancel)
                    .await
                    .is_err()
            );
            let mut result = ToolOutput::text("handled leaf error");
            result.final_answer = Some(Answer::Text("done".into()));
            result
        }
    }
    let provider = FnProvider::new(|req: &ModelRequest| {
        assert_ne!(req.model, "leaf", "leaf provider panic");
        Ok(response(vec![call("a")], StopReason::ToolUse))
    });
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![Arc::new(LeafPanicTool)],
        Limits::default(),
    );
    let outcome = rt.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    assert_eq!(rt.ledger().snapshot(1).reserved, 0);
    assert!(outcome.usage_subtree.total() > outcome.usage_self.total());
    let events = drain(rx);
    assert!(events.iter().any(|event| matches!(event,
        TraceEvent::NodeEnd { outcome } if outcome.node == 1 && outcome.status == Status::Failed)));
    assert!(events.iter().any(|event| matches!(
        event,
        TraceEvent::SessionEnd {
            status: Status::Failed
        }
    )));
}

struct ConnectingProvider {
    leaf_only: bool,
    sent: bool,
    responsive: bool,
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl ModelProvider for ConnectingProvider {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        if self.leaf_only && req.model == "root" {
            return FnProvider::new(|req: &ModelRequest| {
                Ok(if req.messages.len() == 1 {
                    response(vec![call("connect")], StopReason::ToolUse)
                } else {
                    response(vec![text("done")], StopReason::EndTurn)
                })
            })
            .stream(req, cancel)
            .await;
        }
        self.entered.notify_one();
        if !self.responsive {
            return std::future::pending().await;
        }
        cancel.cancelled().await;
        // Classification may need a little cleanup after observing cancellation.
        tokio::time::sleep(Duration::from_millis(10)).await;
        Err(ProviderError::cancelled(self.sent))
    }
}
struct CancelLeafOwner;
#[async_trait]
impl Tool for CancelLeafOwner {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _input: Value, cx: ToolCx) -> ToolOutput {
        let owner = CancellationToken::new();
        let cancel = owner.clone();
        let watcher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        assert!(
            cx.node
                .llm(LlmCall::new("connecting"), &owner)
                .await
                .is_err()
        );
        watcher.await.unwrap();
        ToolOutput::text("handled cancellation")
    }
}
#[tokio::test]
async fn leaf_owner_cancellation_before_sending_charges_zero_to_root() {
    let (rt, rx) = runtime(
        Arc::new(ConnectingProvider {
            leaf_only: true,
            sent: false,
            responsive: true,
            entered: Arc::new(tokio::sync::Notify::new()),
        }),
        vec![Arc::new(CancelLeafOwner)],
        Limits::default(),
    );
    let outcome = rt.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.usage_subtree, outcome.usage_self);
    assert_eq!(rt.ledger().snapshot(0).used, 240);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    assert!(drain(rx).iter().any(|event| matches!(
        event,
        TraceEvent::AttemptEnd {
            node: 1,
            charged: 0,
            ..
        }
    )));
}
#[tokio::test]
async fn root_cancellation_waits_for_send_classification_with_bounded_grace() {
    for (sent, responsive) in [(false, true), (true, true), (false, false)] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let (rt, rx) = runtime(
            Arc::new(ConnectingProvider {
                leaf_only: false,
                sent,
                responsive,
                entered: entered.clone(),
            }),
            vec![],
            Limits::default(),
        );
        let active = rt.clone();
        let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
        entered.notified().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        rt.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.status, Status::Cancelled);
        let events = drain(rx);
        let reserved = events
            .iter()
            .find_map(|event| match event {
                TraceEvent::AttemptStart { reserved, .. } => Some(*reserved),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            rt.ledger().snapshot(0).used,
            if !sent && responsive { 0 } else { reserved }
        );
        assert_eq!(rt.ledger().snapshot(0).reserved, 0);
    }
}
#[tokio::test]
async fn root_deadline_before_sending_charges_zero() {
    let (rt, _) = runtime(
        Arc::new(ConnectingProvider {
            leaf_only: false,
            sent: false,
            responsive: true,
            entered: Arc::new(tokio::sync::Notify::new()),
        }),
        vec![],
        Limits {
            run_timeout: Duration::from_millis(50),
            ..Limits::default()
        },
    );
    assert_eq!(rt.run(spec()).await.unwrap().status, Status::Timeout);
    assert_eq!(rt.ledger().snapshot(0).used, 0);
    assert_eq!(rt.ledger().snapshot(0).reserved, 0);
}

struct FinishingMutation {
    entered: Arc<tokio::sync::Notify>,
    finished: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for FinishingMutation {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, _input: Value, cx: ToolCx) -> ToolOutput {
        self.entered.notify_one();
        cx.cancel.cancelled().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.finished.store(1, Ordering::SeqCst);
        ToolOutput::text("mutation finished")
    }
}
#[tokio::test]
async fn started_mutation_reports_real_outcome_before_session_end() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(AtomicUsize::new(0));
    let provider = FnProvider::new(|_| Ok(response(vec![call("mutate")], StopReason::ToolUse)));
    let (rt, rx) = runtime(
        Arc::new(provider),
        vec![Arc::new(FinishingMutation {
            entered: entered.clone(),
            finished: finished.clone(),
        })],
        Limits::default(),
    );
    let active = rt.clone();
    let task = tokio::spawn(async move { active.run(spec()).await.unwrap() });
    entered.notified().await;
    rt.cancel();
    assert_eq!(task.await.unwrap().status, Status::Cancelled);
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    let events = drain(rx);
    let result = events.iter().position(|event| matches!(event,
        TraceEvent::ToolResult { content, is_error: false, .. } if content == "mutation finished"
    )).unwrap();
    let end = events
        .iter()
        .position(|event| matches!(event, TraceEvent::SessionEnd { .. }))
        .unwrap();
    assert!(result < end);
}

#[tokio::test]
async fn total_timeout_cancels_cooperative_providers_and_retries_with_send_charges() {
    struct TimeoutProvider {
        sent: bool,
        cancelled: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ModelProvider for TimeoutProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn stream(
            &self,
            _: ModelRequest,
            cancel: CancellationToken,
        ) -> Result<EventStream, ProviderError> {
            let cancelled = self.cancelled.clone();
            if !self.sent {
                cancel.cancelled().await;
                cancelled.fetch_add(1, Ordering::SeqCst);
                return Err(ProviderError::cancelled(false));
            }
            let prefix = stream::iter(vec![
                Ok(StreamEvent::MessageStart {
                    id: None,
                    model: "test".into(),
                    usage: Usage::default(),
                }),
                Ok(StreamEvent::BlockStart {
                    index: 0,
                    block: BlockStart::Text,
                }),
            ]);
            let deltas = stream::unfold(
                (cancel, cancelled, false),
                |(cancel, cancelled, done)| async move {
                    if done {
                        return None;
                    }
                    let event = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled.fetch_add(1, Ordering::SeqCst);
                            Err(ProviderError::cancelled(true))
                        },
                        _ = tokio::time::sleep(Duration::from_millis(5)) => Ok(StreamEvent::TextDelta {
                            index: 0,
                            text: "x".into(),
                        }),
                    };
                    let done = event.is_err();
                    Some((event, (cancel, cancelled, done)))
                },
            );
            Ok(Box::pin(prefix.chain(deltas)))
        }
        async fn model_info(&self, _: &str) -> Result<ModelInfo, ProviderError> {
            Ok(ModelInfo::default())
        }
    }
    for sent in [false, true] {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let (rt, rx) = runtime(
            Arc::new(TimeoutProvider {
                sent,
                cancelled: cancelled.clone(),
            }),
            vec![],
            Limits {
                request_idle: Duration::from_secs(1),
                request_total: Duration::from_millis(50),
                ..Limits::default()
            },
        );
        assert_eq!(rt.run(spec()).await.unwrap().status, Status::Failed);
        // A timeout alone cannot satisfy this assertion: the provider must see
        // the attempt token fire on every retry, including pre-send attempts.
        assert_eq!(cancelled.load(Ordering::SeqCst), 5);
        let ends = drain(rx)
            .into_iter()
            .filter_map(|event| match event {
                TraceEvent::AttemptEnd {
                    outcome, charged, ..
                } => Some((outcome, charged)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ends.len(), 5);
        for (outcome, charged) in ends {
            if sent {
                assert_eq!(outcome, ProviderError::IdleTimeout.to_string());
                assert!(charged > 0);
            } else {
                assert!(outcome.starts_with("not sent:"), "{outcome}");
                assert_eq!(charged, 0);
            }
        }
        assert_eq!(rt.ledger().snapshot(0).reserved, 0);
        assert_eq!(rt.ledger().snapshot(0).used == 0, !sent);
    }
}
