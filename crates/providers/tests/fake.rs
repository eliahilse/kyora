use std::sync::Arc;

use futures::StreamExt;
use kyora_protocol::{
    ContentBlock, Message, ModelRequest, ModelResponse, RequestMeta, StopReason, StreamEvent, Usage,
};
use kyora_providers::{
    ModelProvider, ProviderError, collect,
    fake::{FnProvider, Matcher, Rule, ScriptedProvider},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn request(task: &str) -> ModelRequest {
    ModelRequest {
        model: "fake-model".into(),
        system: None,
        messages: vec![Message::user_text(task)],
        tools: vec![],
        max_tokens: 100,
        metadata: RequestMeta::default(),
    }
}

fn response(text: &str) -> ModelResponse {
    ModelResponse {
        id: None,
        model: String::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

async fn run(provider: &impl ModelProvider, request: ModelRequest) -> ModelResponse {
    collect(
        provider
            .stream(request, CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn first_matching_rule_uses_all_filters_and_last_user_text() {
    let provider = ScriptedProvider::new(vec![
        Rule {
            matcher: Some(Matcher {
                depth: Some(2),
                last_user_contains: Some("follow-up".into()),
                system_contains: Some("system".into()),
            }),
            responses: vec![response("specific")],
        },
        Rule {
            matcher: None,
            responses: vec![response("fallback")],
        },
    ]);
    let mut req = request("first task");
    req.system = Some("system prompt".into());
    req.metadata.depth = 2;
    req.messages.extend([
        Message::user_text("follow-up task"),
        Message::assistant_text("latest assistant"),
    ]);
    assert_eq!(
        run(&provider, req.clone()).await.content,
        response("specific").content
    );
    assert_eq!(
        run(&provider, req.clone()).await.content,
        response("fallback").content
    );
    let error = provider
        .stream(req, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, ProviderError::Other(ref text) if text.starts_with("fake provider: no scripted response for ") && text.contains("follow-up task"))
    );

    for (task, depth, system) in [
        ("no substring", 2, Some("system")),
        ("follow-up", 1, Some("system")),
        ("follow-up", 2, Some("other")),
        ("follow-up", 2, None),
    ] {
        let mut req = request(task);
        req.metadata.depth = depth;
        req.system = system.map(str::to_owned);
        assert_eq!(
            run(&provider, req).await.content,
            response("fallback").content
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_conversations_each_consume_their_own_sequence() {
    let provider = Arc::new(ScriptedProvider::new(vec![Rule {
        matcher: None,
        responses: vec![response("one"), response("two"), response("three")],
    }]));
    let mut tasks = Vec::new();
    for index in 0..64 {
        let provider = Arc::clone(&provider);
        tasks.push(tokio::spawn(async move {
            let mut req = request(&format!("task {index}"));
            for expected in ["one", "two", "three"] {
                tokio::task::yield_now().await;
                let actual = run(provider.as_ref(), req.clone()).await;
                assert_eq!(actual.content, response(expected).content);
                req.messages.push(Message::assistant_text(expected));
                req.messages.push(Message::user_text("continue"));
            }
            assert!(matches!(
                provider.stream(req, CancellationToken::new()).await,
                Err(ProviderError::Other(_))
            ));
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn system_and_complete_first_message_define_conversation_keys() {
    let provider = ScriptedProvider::new(vec![Rule {
        matcher: None,
        responses: vec![response("one"), response("two")],
    }]);
    let req = request("same task");
    let mut other_system = req.clone();
    other_system.system = Some("different system".into());
    let mut other_first_block = req.clone();
    other_first_block.messages[0]
        .content
        .push(ContentBlock::Opaque {
            provider: "test".into(),
            kind: "context".into(),
            raw: json!({"id": 1}),
        });
    for conversation in [req, other_system, other_first_block] {
        assert_eq!(
            run(&provider, conversation.clone()).await.content,
            response("one").content
        );
        assert_eq!(
            run(&provider, conversation).await.content,
            response("two").content
        );
    }
}

#[tokio::test]
async fn fixture_loads_minimal_responses_and_uses_request_model() {
    let provider = ScriptedProvider::from_json(include_str!("fixtures/script.json")).unwrap();
    let req = request("hello");
    let first = run(&provider, req.clone()).await;
    assert_eq!(first.model, "fake-model");
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    assert_eq!(
        first.content,
        vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "echo".into(),
            input: json!({"text": "hello"})
        }]
    );
    assert_eq!(run(&provider, req).await.content, response("done").content);
    assert!(matches!(
        ScriptedProvider::from_json("not json"),
        Err(ProviderError::Protocol(_))
    ));
}

#[tokio::test]
async fn canned_blocks_and_usage_survive_chunked_streaming() {
    let canned = ModelResponse {
        id: Some("id".into()),
        model: "specified-model".into(),
        content: vec![
            ContentBlock::Text {
                text: "héllo 🦀".into(),
            },
            ContentBlock::Thinking {
                thinking: "think\nverbatim".into(),
                signature: Some("sig==".into()),
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: Some(String::new()),
            },
            ContentBlock::Thinking {
                thinking: "unsigned".into(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "call".into(),
                name: "echo".into(),
                input: json!({"text": "héllo", "n": 3}),
            },
            ContentBlock::Opaque {
                provider: "test".into(),
                kind: "compaction".into(),
                raw: json!({"untouched": [1, null]}),
            },
        ],
        stop_reason: StopReason::Other("future_reason".into()),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 40,
        },
    };
    let provider = FnProvider::new(|_: &ModelRequest| Ok(canned.clone())).with_chunk_size(1);
    let events = provider
        .stream(request("task"), CancellationToken::new())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.first(),
        Some(Ok(StreamEvent::MessageStart { .. }))
    ));
    assert!(matches!(events.last(), Some(Ok(StreamEvent::MessageStop))));
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::TextDelta { .. })))
            .count()
            > 1
    );
    assert_eq!(run(&provider, request("task")).await, canned);
}

#[tokio::test]
async fn usage_is_synthesized_from_text_byte_lengths_with_minimum_one() {
    let provider = FnProvider::new(|req: &ModelRequest| {
        assert_eq!(req.metadata.node_id.as_deref(), Some("node"));
        Ok(response("héllo!!"))
    });
    let mut req = request("12345678");
    req.system = Some("1234".into());
    req.metadata.node_id = Some("node".into());
    let usage = run(&provider, req).await.usage;
    assert_eq!(
        usage,
        Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        }
    );
    let provider = FnProvider::new(|_: &ModelRequest| Ok(response("")));
    assert_eq!(
        run(&provider, request("")).await.usage,
        Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Usage::default()
        }
    );
}

#[tokio::test]
async fn cancellation_before_stream_preserves_queue_and_during_stream_reports_error() {
    let provider = ScriptedProvider::new(vec![Rule {
        matcher: None,
        responses: vec![response("one response")],
    }])
    .with_chunk_size(0);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        provider.stream(request("task"), cancelled).await,
        Err(ProviderError::Cancelled)
    ));
    let cancel = CancellationToken::new();
    let mut events = provider
        .stream(request("task"), cancel.clone())
        .await
        .unwrap();
    assert!(matches!(
        events.next().await,
        Some(Ok(StreamEvent::MessageStart { .. }))
    ));
    cancel.cancel();
    assert!(matches!(
        events.next().await,
        Some(Err(ProviderError::Cancelled))
    ));
    assert!(events.next().await.is_none());
    let provider = FnProvider::new(|_: &ModelRequest| Ok(response("unused")));
    assert!(matches!(
        provider.stream(request("task"), cancel).await,
        Err(ProviderError::Cancelled)
    ));
}

#[tokio::test]
async fn closure_errors_and_unstreamable_tool_results_are_reported() {
    let provider =
        FnProvider::new(|_: &ModelRequest| Err(ProviderError::ContextTooLarge("test".into())));
    assert!(matches!(
        provider
            .stream(request("task"), CancellationToken::new())
            .await,
        Err(ProviderError::ContextTooLarge(_))
    ));
    let provider = FnProvider::new(|_: &ModelRequest| {
        Ok(ModelResponse {
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: vec![],
                is_error: true,
            }],
            ..response("")
        })
    });
    assert!(matches!(
        provider
            .stream(request("task"), CancellationToken::new())
            .await,
        Err(ProviderError::Protocol(_))
    ));
}
