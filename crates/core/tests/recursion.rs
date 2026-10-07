use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use kyora_core::{
    AgentHandle, AgentOutcome, AgentSpec, Answer, ChildSpec, Effect, Limits, LlmCall, NodeCtx,
    Owner, RecursionError, Runtime, RuntimeConfig, Status, Tool, ToolCx, ToolOutput, ToolSelection,
    Toolset, ToolsetFactory,
    runtime::NodeInfo,
    session::SessionStore,
    trace::{TraceEvent, TraceRecord, TraceSink, reconstruct_jsonl, reconstruct_tree},
};
use kyora_protocol::{ContentBlock, ModelRequest, ModelResponse, StopReason, ToolSpec, Usage};
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
fn response(input: Option<Value>) -> ModelResponse {
    ModelResponse {
        content: match &input {
            Some(input) => vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "python".into(),
                input: input.clone(),
            }],
            None => vec![ContentBlock::Text {
                text: "done".into(),
            }],
        },
        stop_reason: if input.is_some() {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        },
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 7,
        },
        id: None,
        model: String::new(),
        usage_iterations: vec![],
    }
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
fn root_rule(input: Value) -> Rule {
    rule("root task", 0, vec![response(Some(input)), response(None)])
}
fn setup(
    provider: Arc<dyn ModelProvider>,
    toolsets: Arc<dyn ToolsetFactory>,
    limits: Limits,
    trace: TraceSink,
) -> Runtime {
    Runtime::new(RuntimeConfig {
        providers: BTreeMap::from([("fake".into(), provider)]),
        toolsets,
        limits,
        retry: RetryPolicy {
            base: Duration::ZERO,
            ..RetryPolicy::default()
        },
        llm_model: "fake/leaf".parse().unwrap(),
        trace,
        session: "recursion".into(),
    })
    .unwrap()
}
fn spec() -> AgentSpec {
    let mut spec = AgentSpec::new("root task", std::env::current_dir().unwrap());
    spec.model = "fake/agent".parse().unwrap();
    spec
}
fn finish() -> ToolOutput {
    let mut result = ToolOutput::text("ok");
    result.final_answer = Some(Answer::Text("done".into()));
    result
}
fn records(mut rx: tokio::sync::broadcast::Receiver<TraceRecord>) -> Vec<TraceRecord> {
    let mut records = vec![];
    while let Ok(record) = rx.try_recv() {
        records.push(record);
    }
    records
}
fn assert_order(records: &[TraceRecord]) {
    let ends = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            TraceEvent::NodeEnd { outcome } => Some((outcome.node, index)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for record in records {
        if let TraceEvent::NodeStart {
            node,
            parent: Some(parent),
            ..
        } = record.event
        {
            assert!(
                ends[&node] < ends[&parent],
                "child {node} must end before parent {parent}"
            );
        }
    }
}

#[tokio::test]
async fn three_levels_exact_usage_shared_results_and_written_tree() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::create(home.path()).unwrap();
    let rx = store.trace.subscribe();
    let outcomes = Arc::new(Mutex::new(Vec::<AgentOutcome>::new()));
    let saved = outcomes.clone();
    let spawn = tool(move |input, cx| {
        let saved = saved.clone();
        async move {
            let names = input["children"].as_array().unwrap();
            let children = names
                .iter()
                .map(|name| {
                    let task = name.as_str().unwrap();
                    let mut child = ChildSpec::new(task);
                    child.name = Some(task.into());
                    child.origin_cell = Some(4);
                    cx.node.spawn_agent(child, Owner::Node).unwrap()
                })
                .collect::<Vec<_>>();
            // Leaf calls run even at maximum agent depth, and use this scope's budget.
            let leaf = cx
                .node
                .llm(LlmCall::new("leaf task"), &cx.cancel)
                .await
                .unwrap();
            assert_eq!(leaf.response.usage.total(), 130);
            for child in children {
                let copy = child.clone();
                let (a, b) = tokio::join!(child.result(), copy.result());
                assert_eq!(
                    serde_json::to_value(&a).unwrap(),
                    serde_json::to_value(&b).unwrap()
                );
                assert!(child.is_finished());
                assert_eq!(child.status().status, Some(Status::Completed));
                assert_eq!(child.status().usage_subtree, a.usage_subtree);
                saved.lock().unwrap().push(a);
            }
            ToolOutput::text("children complete")
        }
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        root_rule(json!({"children":["child-a", "child-b"]})),
        rule(
            "child-a",
            1,
            vec![
                response(Some(json!({"children":["grand-a"]}))),
                response(None),
            ],
        ),
        rule(
            "child-b",
            1,
            vec![
                response(Some(json!({"children":["grand-b"]}))),
                response(None),
            ],
        ),
        rule(
            "grand-",
            2,
            vec![response(Some(json!({"children":[]}))), response(None)],
        ),
        Rule {
            matcher: Some(Matcher {
                first_user_contains: Some("leaf task".into()),
                ..Matcher::default()
            }),
            responses: vec![response(None)],
        },
    ]));
    let runtime = setup(
        provider,
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            max_inflight_requests: 1,
            ..Limits::default()
        },
        store.trace.clone(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    store.trace.finish().await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.usage_self.total(), 260);
    assert_eq!(outcome.usage_subtree.total(), 1950); // Five agents with two turns and five leaves.
    for child in outcomes.lock().unwrap().iter() {
        assert_eq!(child.usage_self.total(), 260);
        assert_eq!(
            child.usage_subtree.total(),
            if child.node <= 2 { 780 } else { 390 }
        );
    }
    let live = records(rx);
    assert_order(&live);
    let total = live
        .iter()
        .filter_map(|r| match r.event {
            TraceEvent::AttemptEnd { charged, .. } => Some(charged),
            _ => None,
        })
        .sum::<u64>();
    assert_eq!(total, outcome.usage_subtree.total());
    let tree = reconstruct_tree(&live).unwrap();
    assert_eq!(tree.len(), 1);
    let agents = tree[0]
        .children
        .iter()
        .filter(|child| child.kind == "agent")
        .collect::<Vec<_>>();
    assert_eq!(agents.len(), 2);
    for child in agents {
        assert_eq!(child.origin_cell, Some(4));
        assert_eq!(
            child
                .children
                .iter()
                .filter(|child| child.kind == "agent")
                .count(),
            1
        );
        assert_eq!(child.usage_subtree.total(), 780);
    }
    let written = std::fs::read_to_string(store.path.join("events.jsonl")).unwrap();
    assert_eq!(tree, reconstruct_jsonl(&written).unwrap());
    assert_eq!(tree[0].usage_subtree, outcome.usage_subtree);
    let prompts = live
        .iter()
        .filter_map(|r| match &r.event {
            TraceEvent::NodeStart {
                parent: Some(0),
                kind,
                system,
                ..
            } if kind == "agent" => system.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn depth_limit_is_typed_and_leaf_calls_still_work() {
    let spawn = tool(|_, cx| async move {
        if cx.node.depth == 0 {
            let child = cx
                .node
                .spawn_agent(ChildSpec::new("child"), Owner::Node)
                .unwrap();
            assert_eq!(child.result().await.status, Status::Completed);
        } else {
            assert!(matches!(
                cx.node.spawn_agent(ChildSpec::new("too deep"), Owner::Node),
                Err(RecursionError::LimitExceeded { limit: "depth" })
            ));
            cx.node.llm(LlmCall::new("leaf"), &cx.cancel).await.unwrap();
        }
        finish()
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        root_rule(json!({})),
        rule("child", 1, vec![response(Some(json!({})))]),
        rule("leaf", 1, vec![response(None)]),
    ]));
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider,
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            max_depth: 1,
            ..Limits::default()
        },
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(reconstruct_tree(&records(rx)).unwrap()[0].children.len(), 1);
}

/// Uses scripted responses normally; blocked tasks wait for cancellation before any send.
struct BlockingProvider {
    scripted: ScriptedProvider,
    started: tokio::sync::Notify,
}
#[async_trait]
impl ModelProvider for BlockingProvider {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        if request.messages[0].text().contains("blocked") {
            self.started.notify_one();
            cancel.cancelled().await;
            Err(ProviderError::cancelled(false))
        } else {
            self.scripted.stream(request, cancel).await
        }
    }
}
fn blocked(rules: Vec<Rule>) -> Arc<BlockingProvider> {
    Arc::new(BlockingProvider {
        scripted: ScriptedProvider::new(rules),
        started: tokio::sync::Notify::new(),
    })
}

#[tokio::test]
async fn live_and_total_limits_count_root_and_release_after_shutdown() {
    let spawn = tool(|_, cx| async move {
        let a = cx
            .node
            .spawn_agent(ChildSpec::new("blocked first"), Owner::Node)
            .unwrap();
        assert!(matches!(
            cx.node
                .spawn_agent(ChildSpec::new("blocked second"), Owner::Node),
            Err(RecursionError::LimitExceeded {
                limit: "agents_live"
            })
        ));
        a.cancel();
        assert_eq!(a.result().await.status, Status::Cancelled);
        let b = cx
            .node
            .spawn_agent(ChildSpec::new("blocked second"), Owner::Node)
            .unwrap();
        b.cancel();
        b.result().await;
        assert!(matches!(
            cx.node
                .spawn_agent(ChildSpec::new("blocked third"), Owner::Node),
            Err(RecursionError::LimitExceeded {
                limit: "agents_total"
            })
        ));
        finish()
    });
    let runtime = setup(
        blocked(vec![root_rule(json!({}))]),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            max_agents_live: 2,
            max_agents_total: 3,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
}

#[tokio::test]
async fn concurrent_spawns_admit_exactly_the_available_live_and_total_slots() {
    for (max_agents_live, max_agents_total, expected_limit) in
        [(4, 100, "agents_live"), (100, 4, "agents_total")]
    {
        let spawn = tool(move |_, cx| async move {
            let mut tasks = vec![];
            let barrier = Arc::new(tokio::sync::Barrier::new(16));
            for _ in 0..16 {
                let node = cx.node.clone();
                let barrier = barrier.clone();
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    node.spawn_agent(ChildSpec::new("blocked child"), Owner::Node)
                }));
            }
            let mut admitted = vec![];
            for task in tasks {
                match task.await.unwrap() {
                    Ok(handle) => admitted.push(handle),
                    Err(RecursionError::LimitExceeded { limit }) if limit == expected_limit => {}
                    _ => panic!("unexpected admission result"),
                }
            }
            assert_eq!(admitted.len(), 3);
            for handle in admitted {
                handle.cancel();
                assert_eq!(handle.result().await.status, Status::Cancelled);
            }
            finish()
        });
        let runtime = setup(
            blocked(vec![root_rule(json!({}))]),
            Arc::new(Toolset::new(vec![spawn]).unwrap()),
            Limits {
                max_agents_live,
                max_agents_total,
                ..Limits::default()
            },
            TraceSink::ephemeral(),
        );
        runtime.run(spec()).await.unwrap();
    }
}

#[tokio::test]
async fn subtree_budget_exhaustion_does_not_stop_sibling() {
    let spawn = tool(|_, cx| async move {
        let mut poor = ChildSpec::new("poor child");
        poor.budget = Some(1);
        let poor = cx.node.spawn_agent(poor, Owner::Node).unwrap();
        let good = cx
            .node
            .spawn_agent(ChildSpec::new("good child"), Owner::Node)
            .unwrap();
        let (poor, good) = tokio::join!(poor.result(), good.result());
        assert_eq!(poor.status, Status::BudgetExhausted);
        assert_eq!(poor.usage_subtree.total(), 0);
        assert_eq!(good.status, Status::Completed);
        assert_eq!(good.usage_subtree.total(), 130);
        finish()
    });
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("child", 1, vec![response(None)]),
        ])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        TraceSink::ephemeral(),
    );
    assert_eq!(
        runtime.run(spec()).await.unwrap().usage_subtree.total(),
        260
    );
}

#[tokio::test]
async fn child_deadline_times_out_and_parent_completes() {
    let spawn = tool(|_, cx| async move {
        let mut child = ChildSpec::new("blocked child");
        child.timeout = Some(Duration::from_millis(30));
        let child = cx.node.spawn_agent(child, Owner::Node).unwrap();
        assert!(!child.is_finished());
        assert_eq!(child.status().status, None);
        let result = child.result().await;
        assert_eq!(result.status, Status::Timeout);
        finish()
    });
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        blocked(vec![root_rule(json!({}))]),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_order(&records(rx));
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn cell_owner_cancels_subtree_while_node_sibling_continues() {
    let provider = blocked(vec![
        root_rule(json!({})),
        rule("cell child", 1, vec![response(Some(json!({})))]),
        rule("sibling", 1, vec![response(None)]),
    ]);
    let child_started = provider.clone();
    let spawn = tool(move |_, cx| {
        let child_started = child_started.clone();
        async move {
            if cx.node.depth == 0 {
                let cell = CancellationToken::new();
                let child = cx
                    .node
                    .spawn_agent(ChildSpec::new("cell child"), Owner::Cell(cell.clone()))
                    .unwrap();
                // The notify proves the grandchild has entered its blocked request.
                child_started.started.notified().await;
                let sibling = cx
                    .node
                    .spawn_agent(ChildSpec::new("sibling"), Owner::Node)
                    .unwrap();
                cell.cancel();
                assert_eq!(child.result().await.status, Status::Cancelled);
                assert_eq!(sibling.result().await.status, Status::Completed);
                finish()
            } else {
                let grand = cx
                    .node
                    .spawn_agent(ChildSpec::new("blocked grandchild"), Owner::Node)
                    .unwrap();
                assert_eq!(grand.result().await.status, Status::Cancelled);
                ToolOutput::text("cancelled")
            }
        }
    });
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider,
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    let records = records(rx);
    assert_order(&records);
    let tree = reconstruct_tree(&records).unwrap();
    assert_eq!(
        tree[0].children[0].children[0].status,
        Some(Status::Cancelled)
    );
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn parent_end_cancels_and_joins_node_owned_children_and_closes_admission() {
    let saved = Arc::new(Mutex::new(None::<(AgentHandle, NodeCtx)>));
    let saved_tool = saved.clone();
    let provider = blocked(vec![root_rule(json!({}))]);
    let started = provider.clone();
    let spawn = tool(move |_, cx| {
        let saved = saved_tool.clone();
        let started = started.clone();
        async move {
            let child = cx
                .node
                .spawn_agent(ChildSpec::new("blocked child"), Owner::Node)
                .unwrap();
            started.started.notified().await;
            *saved.lock().unwrap() = Some((child, cx.node));
            finish()
        }
    });
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider,
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    let (child, node) = saved.lock().unwrap().take().unwrap();
    assert!(child.is_finished());
    assert_eq!(child.result().await.status, Status::Cancelled);
    assert!(matches!(
        node.spawn_agent(ChildSpec::new("late"), Owner::Node),
        Err(RecursionError::Cancelled)
    ));
    assert!(matches!(
        node.llm(LlmCall::new("late"), &CancellationToken::new())
            .await,
        Err(RecursionError::Cancelled)
    ));
    assert_order(&records(rx));
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn selection_attenuation_and_invalid_arguments_do_not_start_children() {
    let spawn = tool(|_, cx| async move {
        for child in [
            ChildSpec {
                tools: ToolSelection(Some(vec!["missing".into()])),
                ..ChildSpec::new("bad")
            },
            ChildSpec {
                max_turns: Some(0),
                ..ChildSpec::new("bad")
            },
            ChildSpec {
                timeout: Some(Duration::ZERO),
                ..ChildSpec::new("bad")
            },
            ChildSpec {
                timeout: Some(Duration::MAX),
                ..ChildSpec::new("bad")
            },
            ChildSpec {
                budget: Some(0),
                ..ChildSpec::new("bad")
            },
            ChildSpec {
                model: Some("other/model".parse().unwrap()),
                ..ChildSpec::new("bad")
            },
        ] {
            assert!(matches!(
                cx.node.spawn_agent(child, Owner::Node),
                Err(RecursionError::InvalidRequest(_))
            ));
        }
        let cell = CancellationToken::new();
        cell.cancel();
        assert!(matches!(
            cx.node
                .spawn_agent(ChildSpec::new("bad"), Owner::Cell(cell)),
            Err(RecursionError::Cancelled)
        ));
        let child = cx
            .node
            .spawn_agent(ChildSpec::new("valid"), Owner::Node)
            .unwrap();
        assert_eq!(child.id, 1);
        assert_eq!(child.result().await.status, Status::Completed);
        finish()
    });
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("valid", 1, vec![response(None)]),
        ])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    runtime.run(spec()).await.unwrap();
    assert_eq!(reconstruct_tree(&records(rx)).unwrap()[0].children.len(), 1);
}

struct InspectFactory {
    infos: Arc<Mutex<Vec<NodeInfo>>>,
    tools: Toolset,
}
impl ToolsetFactory for InspectFactory {
    fn toolset(&self, info: &NodeInfo, selection: &ToolSelection) -> anyhow::Result<Toolset> {
        assert_eq!(info.selection.0, selection.0);
        self.infos.lock().unwrap().push(info.clone());
        // Core must still enforce selection if a factory offers additional tools.
        Ok(self.tools.clone())
    }
}

#[tokio::test]
async fn factory_receives_opaque_init_selection_model_and_preamble() {
    let init = Arc::new(json!({"vars":{"context":[1,2,3]}}));
    let child_init = init.clone();
    let spawn = tool(move |_, cx| {
        let child_init = child_init.clone();
        async move {
            let child = ChildSpec {
                init: Some(child_init),
                preamble: Some("variable manifest".into()),
                model: Some("fake/override".parse().unwrap()),
                tools: ToolSelection(Some(vec![])),
                ..ChildSpec::new("child")
            };
            assert_eq!(
                cx.node
                    .spawn_agent(child, Owner::Node)
                    .unwrap()
                    .result()
                    .await
                    .status,
                Status::Completed
            );
            finish()
        }
    });
    let infos = Arc::new(Mutex::new(vec![]));
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("child\n\nvariable manifest", 1, vec![response(None)]),
        ])),
        Arc::new(InspectFactory {
            infos: infos.clone(),
            tools: Toolset::new(vec![spawn]).unwrap(),
        }),
        Limits::default(),
        trace,
    );
    runtime
        .set_subagent_prompt("custom child prompt".into())
        .unwrap();
    runtime.run(spec()).await.unwrap();
    assert!(runtime.set_subagent_prompt("late".into()).is_err());
    let infos = infos.lock().unwrap();
    assert_eq!(infos.len(), 2);
    assert!(Arc::ptr_eq(infos[1].init.as_ref().unwrap(), &init));
    assert_eq!(infos[1].selection.0, Some(vec![]));
    assert_eq!(infos[1].model.to_string(), "fake/override");
    assert_eq!(infos[1].cwd, infos[0].cwd);
    let live = records(rx);
    assert!(live.iter().any(|r| matches!(&r.event, TraceEvent::NodeStart { node:1, tools, system:Some(system), .. } if tools.is_empty() && system == "custom child prompt")));
}

#[tokio::test]
async fn default_and_overridden_child_turn_caps() {
    let spawn = tool(|_, cx| async move {
        let default = cx
            .node
            .spawn_agent(ChildSpec::new("default child"), Owner::Node)
            .unwrap();
        let custom = cx
            .node
            .spawn_agent(
                ChildSpec {
                    max_turns: Some(2),
                    ..ChildSpec::new("custom child")
                },
                Owner::Node,
            )
            .unwrap();
        let (default, custom) = tokio::join!(default.result(), custom.result());
        assert_eq!((default.status, default.turns), (Status::MaxTurns, 1));
        assert_eq!((custom.status, custom.turns), (Status::MaxTurns, 2));
        finish()
    });
    let pause = ModelResponse {
        stop_reason: StopReason::PauseTurn,
        ..response(None)
    };
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("child", 1, vec![pause.clone(), pause.clone(), pause]),
        ])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            subagent_max_turns: 1,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    runtime.run(spec()).await.unwrap();
}

#[tokio::test]
async fn llm_admission_errors_are_typed_and_do_not_allocate_nodes() {
    let spawn = tool(|_, cx| async move {
        assert!(matches!(
            cx.node
                .llm(
                    LlmCall {
                        max_tokens: Some(0),
                        ..LlmCall::new("invalid")
                    },
                    &cx.cancel
                )
                .await,
            Err(RecursionError::InvalidRequest(_))
        ));
        assert_eq!(
            cx.node
                .llm(LlmCall::new("leaf"), &cx.cancel)
                .await
                .unwrap()
                .node,
            1
        );
        assert!(matches!(
            cx.node.llm(LlmCall::new("second"), &cx.cancel).await,
            Err(RecursionError::LimitExceeded { limit: "llm_calls" })
        ));
        finish()
    });
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("leaf", 0, vec![response(None)]),
        ])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            max_llm_calls: 1,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    runtime.run(spec()).await.unwrap();
}

#[tokio::test]
async fn closed_budget_refuses_agents_and_leaves_before_start() {
    let spawn = tool(|_, cx| async move {
        assert_eq!(cx.node.budget().remaining(), 0);
        assert!(matches!(
            cx.node.spawn_agent(ChildSpec::new("child"), Owner::Node),
            Err(RecursionError::BudgetExceeded)
        ));
        assert!(matches!(
            cx.node.llm(LlmCall::new("leaf"), &cx.cancel).await,
            Err(RecursionError::BudgetExceeded)
        ));
        finish()
    });
    let mut first = response(Some(json!({})));
    first.usage.input_tokens = 100_001;
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![rule(
            "root task",
            0,
            vec![first],
        )])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            budget_tokens: 100_000,
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn child_timeout_is_bounded_by_parent_deadline() {
    let saved = Arc::new(Mutex::new(None));
    let saved_tool = saved.clone();
    let spawn = tool(move |_, cx| {
        let saved = saved_tool.clone();
        async move {
            let child = cx
                .node
                .spawn_agent(
                    ChildSpec {
                        timeout: Some(Duration::from_secs(3600)),
                        ..ChildSpec::new("blocked child")
                    },
                    Owner::Node,
                )
                .unwrap();
            *saved.lock().unwrap() = Some(child.clone());
            assert_eq!(child.result().await.status, Status::Timeout);
            ToolOutput::text("timed out")
        }
    });
    let runtime = setup(
        blocked(vec![root_rule(json!({}))]),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits {
            run_timeout: Duration::from_millis(50),
            ..Limits::default()
        },
        TraceSink::ephemeral(),
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Timeout);
    let child = saved.lock().unwrap().take().unwrap();
    assert!(child.is_finished());
    assert_eq!(child.status().status, Some(Status::Timeout));
}

#[test]
fn reconstruction_handles_partial_traces_and_rejects_invalid_ancestry() {
    let start = |node, parent| TraceRecord {
        v: 1,
        seq: None,
        ts: String::new(),
        event: TraceEvent::NodeStart {
            node,
            parent,
            depth: 0,
            kind: "agent".into(),
            name: String::new(),
            model: "fake/model".into(),
            origin_cell: None,
            system: None,
            tools: vec![],
            limits: Limits::default(),
            prompt: None,
        },
    };
    let mut partial = vec![start(1, Some(0)), start(0, None)];
    partial.push(TraceRecord {
        v: 1,
        seq: None,
        ts: String::new(),
        event: TraceEvent::AttemptEnd {
            node: 1,
            attempt: 0,
            request_id: None,
            outcome: "cancelled".into(),
            charged: 70,
            excess: 0,
            usage: None,
            stop_reason: None,
            ms: 0,
        },
    });
    // Repeated settlement records never count twice.
    partial.push(partial[2].clone());
    let tree = reconstruct_tree(&partial).unwrap();
    assert_eq!(tree[0].status, None);
    assert_eq!(tree[0].usage_subtree.total(), 70);
    assert!(reconstruct_tree(&[start(1, Some(2))]).is_err());
    assert!(reconstruct_tree(&[start(1, Some(2)), start(2, Some(1))]).is_err());
    assert!(reconstruct_tree(&[start(0, None), start(0, None)]).is_err());
}

struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            input_schema: json!({"type":"object"}),
            ..ToolSpec::default()
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _: Value, _: ToolCx) -> ToolOutput {
        ToolOutput::text("read")
    }
}

#[tokio::test]
async fn child_cannot_reacquire_factory_tool_removed_from_parent() {
    let spawn = tool(|_, cx| async move {
        let child = ChildSpec {
            tools: ToolSelection(Some(vec!["read_file".into()])),
            ..ChildSpec::new("invalid")
        };
        assert!(matches!(
            cx.node.spawn_agent(child, Owner::Node),
            Err(RecursionError::InvalidRequest(_))
        ));
        let child = cx
            .node
            .spawn_agent(ChildSpec::new("valid"), Owner::Node)
            .unwrap();
        assert_eq!(child.result().await.status, Status::Completed);
        finish()
    });
    let infos = Arc::new(Mutex::new(vec![]));
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule("valid", 1, vec![response(None)]),
        ])),
        Arc::new(InspectFactory {
            infos: infos.clone(),
            tools: Toolset::new(vec![spawn, Arc::new(ReadTool)]).unwrap(),
        }),
        Limits::default(),
        trace,
    );
    let mut root = spec();
    root.tools = ToolSelection(Some(vec!["python".into()]));
    runtime.run(root).await.unwrap();
    for record in records(rx) {
        if let TraceEvent::NodeStart { tools, .. } = record.event {
            assert_eq!(
                tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["python"]
            );
        }
    }
    assert_eq!(
        infos.lock().unwrap()[1].selection.0,
        Some(vec!["python".into()])
    );
}

#[tokio::test]
async fn descendant_charges_close_only_the_bounded_subtree() {
    let spawn = tool(|_, cx| async move {
        if cx.node.depth == 0 {
            let limited = cx
                .node
                .spawn_agent(
                    ChildSpec {
                        budget: Some(6000),
                        ..ChildSpec::new("limited child")
                    },
                    Owner::Node,
                )
                .unwrap();
            let sibling = cx
                .node
                .spawn_agent(ChildSpec::new("sibling child"), Owner::Node)
                .unwrap();
            let (limited, sibling) = tokio::join!(limited.result(), sibling.result());
            assert_eq!(limited.status, Status::BudgetExhausted);
            assert_eq!(limited.usage_self.total(), 130);
            assert_eq!(limited.usage_subtree.total(), 6131);
            assert_eq!(sibling.status, Status::Completed);
            finish()
        } else {
            assert_eq!(
                cx.node
                    .llm(LlmCall::new("expensive leaf"), &cx.cancel)
                    .await
                    .unwrap()
                    .response
                    .usage
                    .total(),
                6001
            );
            assert!(matches!(
                cx.node
                    .spawn_agent(ChildSpec::new("grandchild"), Owner::Node),
                Err(RecursionError::BudgetExceeded)
            ));
            assert!(matches!(
                cx.node.llm(LlmCall::new("another leaf"), &cx.cancel).await,
                Err(RecursionError::BudgetExceeded)
            ));
            ToolOutput::text("subtree exhausted")
        }
    });
    let mut expensive = response(None);
    expensive.usage = Usage {
        input_tokens: 6001,
        ..Usage::default()
    };
    let runtime = setup(
        Arc::new(ScriptedProvider::new(vec![
            root_rule(json!({})),
            rule(
                "limited child",
                1,
                vec![response(Some(json!({}))), response(None)],
            ),
            rule("sibling child", 1, vec![response(None)]),
            rule("expensive leaf", 1, vec![expensive]),
        ])),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        TraceSink::ephemeral(),
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.usage_subtree.total(), 6391);
    assert!(!runtime.ledger().snapshot(0).closed);
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

/// Scripted responses, recording the node id of every dispatched request.
struct Counting {
    scripted: ScriptedProvider,
    dispatched: Mutex<Vec<String>>,
}
#[async_trait]
impl ModelProvider for Counting {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        self.dispatched
            .lock()
            .unwrap()
            .push(request.metadata.node_id.clone().unwrap_or_default());
        self.scripted.stream(request, cancel).await
    }
}
fn counting(rules: Vec<Rule>) -> Arc<Counting> {
    Arc::new(Counting {
        scripted: ScriptedProvider::new(rules),
        dispatched: Mutex::new(vec![]),
    })
}

#[tokio::test]
async fn independent_cell_child_dispatches_nothing_after_its_parent_ends() {
    // The parent ends without yielding, so the child's task has not started and
    // nothing has propagated the parent's cancellation to its independent token.
    let spawn = tool(|_, cx| async move {
        cx.node
            .spawn_agent(
                ChildSpec::new("cell child"),
                Owner::Cell(CancellationToken::new()),
            )
            .unwrap();
        finish()
    });
    let provider = counting(vec![
        root_rule(json!({})),
        rule("cell child", 1, vec![response(None)]),
    ]);
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider.clone(),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    assert_eq!(runtime.run(spec()).await.unwrap().status, Status::Completed);
    assert_eq!(*provider.dispatched.lock().unwrap(), vec!["0"]);
    let live = records(rx);
    assert!(
        !live
            .iter()
            .any(|record| matches!(record.event, TraceEvent::AttemptStart { node: 1, .. }))
    );
    assert_eq!(
        reconstruct_tree(&live).unwrap()[0].children[0].status,
        Some(Status::Cancelled)
    );
    assert_order(&live);
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}

#[tokio::test]
async fn leaf_owner_cancelled_before_its_task_starts_dispatches_nothing() {
    let spawn = tool(|_, cx| async move {
        let owner = CancellationToken::new();
        let mut call = std::pin::pin!(cx.node.llm(LlmCall::new("leaf"), &owner));
        // One poll admits the leaf and queues its owned task without running it.
        assert!(futures::poll!(call.as_mut()).is_pending());
        owner.cancel();
        assert!(matches!(call.await, Err(RecursionError::Cancelled)));
        assert_eq!(cx.node.budget().reserved, 0);
        finish()
    });
    let provider = counting(vec![
        root_rule(json!({})),
        rule("leaf", 0, vec![response(None)]),
    ]);
    let trace = TraceSink::ephemeral();
    let rx = trace.subscribe();
    let runtime = setup(
        provider.clone(),
        Arc::new(Toolset::new(vec![spawn]).unwrap()),
        Limits::default(),
        trace,
    );
    let outcome = runtime.run(spec()).await.unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(*provider.dispatched.lock().unwrap(), vec!["0"]);
    // The leaf still gets its lifecycle records, with nothing charged.
    let live = records(rx);
    assert!(
        !live
            .iter()
            .any(|record| matches!(record.event, TraceEvent::AttemptStart { node: 1, .. }))
    );
    let leaf = &reconstruct_tree(&live).unwrap()[0].children[0];
    assert_eq!(
        (leaf.kind.as_str(), leaf.status),
        ("llm", Some(Status::Cancelled))
    );
    assert_eq!(leaf.usage_self.total(), 0);
    assert_eq!(outcome.usage_subtree.total(), outcome.usage_self.total());
    assert_eq!(runtime.ledger().snapshot(0).reserved, 0);
}
