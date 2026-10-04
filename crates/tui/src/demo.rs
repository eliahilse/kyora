//! Scripted orchestration only. No Python execution, network, or core runtime.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt, future::try_join_all};
use kyora_protocol::{
    ContentBlock, Message, ModelRequest, ModelResponse, RequestMeta, StopReason, StreamEvent, Usage,
};
use kyora_providers::{
    Accumulator, ModelProvider,
    fake::{Rule, ScriptedProvider},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{NodeId, NodeSpec, Status, UiEvent};

pub const PROMPT: &str = "Summarize three incident reports and check their dates.";
pub const CODE: &str = "from concurrent.futures import ThreadPoolExecutor\nwith ThreadPoolExecutor(max_workers=6) as pool:\n    agents = [pool.submit(kyora.agent, task=f'Review report {i}')\n              for i in range(3)]\n    checks = [pool.submit(kyora.llm, prompt=f'Check date {i}')\n              for i in range(3)]\n    results = [f.result() for f in agents + checks]\nprint(results)";
pub const TOKEN_BUDGET: u64 = 20_000;

struct Driver {
    sender: mpsc::Sender<UiEvent>,
    cancel: CancellationToken,
    delay: Duration,
    used: Arc<AtomicU64>,
}

impl Driver {
    async fn emit(&self, event: UiEvent) -> anyhow::Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => anyhow::bail!("cancelled"),
            result = self.sender.send(event) => result.map_err(Into::into),
        }
    }

    async fn pause(&self) -> anyhow::Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => anyhow::bail!("cancelled"),
            _ = tokio::time::sleep(self.delay) => Ok(()),
        }
    }

    async fn response(
        &self,
        node: NodeId,
        model: &str,
        content: Vec<ContentBlock>,
        tokens: u64,
        previous: u64,
    ) -> anyhow::Result<ModelResponse> {
        let stop_reason = if content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let provider = ScriptedProvider::new(vec![Rule {
            matcher: None,
            responses: vec![ModelResponse {
                id: None,
                model: model.into(),
                content,
                stop_reason,
                usage: Usage {
                    input_tokens: tokens - 100,
                    output_tokens: 100,
                    ..Usage::default()
                },
                usage_iterations: vec![],
            }],
        }])
        .with_chunk_size(12);
        let request = ModelRequest {
            model: model.into(),
            system: None,
            messages: vec![Message::user_text(PROMPT)],
            tools: vec![],
            max_tokens: 1024,
            metadata: RequestMeta {
                node_id: Some(node.to_string()),
                depth: u32::from(node != 0),
            },
            options: Default::default(),
        };
        let mut stream = provider.stream(request, self.cancel.child_token()).await?;
        let mut accumulator = Accumulator::new();
        while let Some(event) = stream.next().await {
            let event = event?;
            if let StreamEvent::TextDelta { text, .. } = &event {
                self.emit(UiEvent::TextDelta {
                    node,
                    text: text.clone(),
                })
                .await?;
                self.pause().await?;
            }
            accumulator.push(event)?;
        }
        let response = accumulator.finish()?;
        let charged = response.usage.total();
        self.emit(UiEvent::Usage {
            node,
            tokens: previous + charged,
            cost_microusd: (previous + charged) * 2,
        })
        .await?;
        let used = self.used.fetch_add(charged, Ordering::SeqCst) + charged;
        self.emit(UiEvent::Budget {
            node: None,
            remaining: TOKEN_BUDGET.saturating_sub(used),
        })
        .await?;
        Ok(response)
    }
}

fn text(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text { text: value.into() }
}

fn spec(id: NodeId, parent: Option<NodeId>, name: impl Into<String>, model: &str) -> NodeSpec {
    NodeSpec {
        id,
        parent,
        name: name.into(),
        model: model.into(),
    }
}

/// Plays one offline turn into a bounded event channel. IDs starting at `first_id`
/// must be unused; root is always 0. Existing usage preserves session totals on replay.
pub async fn play(
    sender: mpsc::Sender<UiEvent>,
    cancel: CancellationToken,
    delay: Duration,
    first_id: NodeId,
    session_tokens: u64,
    root_tokens: u64,
) -> anyhow::Result<()> {
    let driver = Driver {
        sender,
        cancel,
        delay,
        used: Arc::new(AtomicU64::new(session_tokens)),
    };
    driver
        .emit(UiEvent::AgentSpawned(spec(0, None, "root", "fake/root")))
        .await?;
    driver
        .emit(UiEvent::Budget {
            node: None,
            remaining: TOKEN_BUDGET.saturating_sub(session_tokens),
        })
        .await?;
    let tool_id = format!("python-{first_id}");
    let response = driver.response(0, "fake/root", vec![
        text("I will split the reports across three agents and check dates with a batch of llm() calls."),
        ContentBlock::ToolUse { id: tool_id.clone(), name: "python".into(), input: serde_json::json!({"code": CODE}) },
    ], 1200, root_tokens).await?;
    for block in &response.content {
        let ContentBlock::ToolUse { name, input, .. } = block else {
            continue;
        };
        driver
            .emit(UiEvent::ToolCallStarted {
                node: 0,
                id: tool_id.clone(),
                name: name.into(),
                args: input["code"].as_str().unwrap_or_default().into(),
            })
            .await?;
    }
    driver
        .emit(UiEvent::ReplCellStarted {
            node: spec(first_id, Some(0), "cell 1", "fake/root"),
            code: CODE.into(),
        })
        .await?;
    let mut calls = Vec::new();
    for index in 0..3 {
        let id = first_id + 1 + index;
        driver
            .emit(UiEvent::AgentSpawned(spec(
                id,
                Some(first_id),
                format!("report {index}"),
                "fake/agent",
            )))
            .await?;
        driver
            .emit(UiEvent::Budget {
                node: Some(id),
                remaining: 3000,
            })
            .await?;
        calls.push((
            id,
            true,
            format!("Report {index}: the incident was resolved. Evidence and dates agree."),
        ));
        driver.pause().await?;
    }
    for index in 0..3 {
        let id = first_id + 4 + index;
        driver
            .emit(UiEvent::LlmCall {
                node: spec(id, Some(first_id), format!("date {index}"), "fake/leaf"),
            })
            .await?;
        calls.push((
            id,
            false,
            format!("Date {index} checked against the source report."),
        ));
        driver.pause().await?;
    }
    try_join_all(calls.into_iter().map(|(id, agent, answer)| {
        let driver = &driver;
        async move {
            driver
                .response(
                    id,
                    if agent { "fake/agent" } else { "fake/leaf" },
                    vec![text(answer)],
                    if agent { 600 } else { 250 },
                    0,
                )
                .await?;
            if agent {
                driver
                    .emit(UiEvent::Budget {
                        node: Some(id),
                        remaining: 2400,
                    })
                    .await?;
            }
            driver
                .emit(if agent {
                    UiEvent::AgentFinished {
                        node: id,
                        status: Status::Done,
                    }
                } else {
                    UiEvent::LlmCallFinished {
                        node: id,
                        status: Status::Done,
                    }
                })
                .await
        }
    }))
    .await?;
    let result = "3 agent reports collected; 3 date checks passed.\nAll incidents resolved. No inconsistent dates.";
    driver
        .emit(UiEvent::ReplCellFinished {
            node: first_id,
            output: result.into(),
            status: Status::Done,
        })
        .await?;
    driver
        .emit(UiEvent::ToolCallFinished {
            node: 0,
            id: tool_id,
            result: result.into(),
            status: Status::Done,
        })
        .await?;
    driver.pause().await?;
    driver.response(0, "fake/root", vec![text("All three incidents are resolved. The agents agree on the evidence, and the three llm() checks found consistent dates. Select a tree node to inspect its work.")], 450, root_tokens + 1200).await?;
    driver
        .emit(UiEvent::AgentFinished {
            node: 0,
            status: Status::Done,
        })
        .await
}
