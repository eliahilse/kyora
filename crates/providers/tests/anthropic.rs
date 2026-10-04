use std::time::Duration;

use futures::StreamExt;
use kyora_protocol::{ContentBlock, Message, ModelRequest, StopReason, StreamEvent, Usage};
use kyora_providers::{
    AttemptCharge, ModelProvider, ProviderError,
    anthropic::{AnthropicConfig, AnthropicProvider, ModelCaps},
    collect,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, headers, method, path},
};

const TEXT: &str = include_str!("fixtures/anthropic-text.sse");
const KEY: &str = "test-only-credential";

fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-haiku-4-5".into(),
        max_tokens: 32,
        messages: vec![Message::user_text("Hi")],
        ..ModelRequest::default()
    }
}

fn config(url: &str) -> AnthropicConfig {
    let mut cfg = AnthropicConfig::new(KEY);
    cfg.base_url = url.into();
    cfg
}

fn provider(url: &str) -> AnthropicProvider {
    AnthropicProvider::new(config(url)).unwrap()
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn start() -> String {
    event(
        json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-haiku-4-5", "usage": {"input_tokens": 12, "output_tokens": 1}}}),
    )
}

fn finish(usage: Value, stop_reason: &str) -> String {
    event(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": usage}))
        + &event(json!({"type": "message_stop"}))
}

async fn mock_stream(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn text_collect_headers_and_private_metadata() {
    let server = MockServer::start().await;
    let mut cfg = config(&server.uri());
    cfg.extra_betas = vec!["custom-beta".into(), "custom-beta".into()];
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .and(header(
            "user-agent",
            concat!("kyora/", env!("CARGO_PKG_VERSION")),
        ))
        .and(headers(
            "anthropic-beta",
            vec![
                "thinking-binding-controls-2026-08-01",
                "task-budgets-2026-03-13",
                "custom-beta",
            ],
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(TEXT.replace('\n', "\r\n"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(cfg).unwrap();
    assert_eq!(provider.name(), "anthropic");
    let mut req = request();
    req.model = "claude-opus-5-5".into();
    req.options.task_budget_total = Some(20_000);
    req.metadata.node_id = Some("private-runtime-node".into());
    req.metadata.depth = 3;
    let response = collect(
        provider
            .stream(req, CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.id.as_deref(), Some("msg_text"));
    assert_eq!(
        response.content,
        vec![ContentBlock::Text {
            text: "Hello 世界".into()
        }]
    );
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 11,
            output_tokens: 4,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30
        }
    );
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("metadata").is_none());
    assert!(!String::from_utf8_lossy(&requests[0].body).contains("private-runtime-node"));
}

#[tokio::test]
async fn no_beta_header_when_none_required() {
    let server = MockServer::start().await;
    mock_stream(&server, TEXT.into()).await;
    collect(
        provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        !server.received_requests().await.unwrap()[0]
            .headers
            .contains_key("anthropic-beta")
    );
}

#[tokio::test]
async fn tool_input_many_deltas_collects_strict_json() {
    let server = MockServer::start().await;
    let mut body = start()
        + &event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "tool_1", "name": "write", "input": {}}}),
        );
    let input = json!({"code": "print('世界')\n", "filename": "test.py"});
    for character in input.to_string().chars() {
        body += &event(
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": character.to_string()}}),
        );
    }
    body += &event(json!({"type": "content_block_stop", "index": 0}));
    body += &finish(json!({"output_tokens": 9}), "tool_use");
    mock_stream(&server, body).await;
    let response = collect(
        provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.content,
        vec![ContentBlock::ToolUse {
            id: "tool_1".into(),
            name: "write".into(),
            input
        }]
    );
}

#[tokio::test]
async fn thinking_signature_and_opaque_blocks_collect_verbatim() {
    let server = MockServer::start().await;
    let mut body = start()
        + &event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        );
    for delta in [
        json!({"type": "thinking_delta", "thinking": "Reason\n"}),
        json!({"type": "signature_delta", "signature": "sig"}),
        json!({"type": "signature_delta", "signature": "nature"}),
        json!({"type": "future_delta", "unknown": 1}),
    ] {
        body += &event(json!({"type": "content_block_delta", "index": 0, "delta": delta}));
    }
    body += &event(json!({"type": "content_block_stop", "index": 0}));
    let mut expected = vec![ContentBlock::Thinking {
        thinking: "Reason\n".into(),
        signature: Some("signature".into()),
    }];
    for (offset, kind) in [
        "redacted_thinking",
        "compaction",
        "fallback",
        "server_tool_use",
        "future_block",
    ]
    .into_iter()
    .enumerate()
    {
        let mut raw = if kind == "server_tool_use" {
            json!({"type": kind, "id": "srvtoolu_1", "name": "web_search", "input": {}})
        } else {
            json!({"type": kind, "data": "verbatim", "extra": {"a": [1, 2]}})
        };
        body += &event(
            json!({"type": "content_block_start", "index": offset + 1, "content_block": raw}),
        );
        if kind == "server_tool_use" {
            // Documented web-search deltas include an empty fragment and split keys.
            for fragment in ["", "{\"query", "\":", " \"weather", " NY", "C to", "day\"}"] {
                body += &event(json!({"type": "content_block_delta", "index": offset + 1,
                    "delta": {"type": "input_json_delta", "partial_json": fragment}}));
            }
            raw["input"] = json!({"query": "weather NYC today"});
        }
        body += &event(json!({"type": "content_block_stop", "index": offset + 1}));
        expected.push(ContentBlock::Opaque {
            provider: "anthropic".into(),
            kind: kind.into(),
            raw,
        });
    }
    body += &event(json!({"type": "future_event", "payload": true}));
    body += &finish(json!({"output_tokens": 10}), "future_stop");
    mock_stream(&server, body).await;
    let response = collect(
        provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.content, expected);
    assert_eq!(
        response.stop_reason,
        StopReason::Other("future_stop".into())
    );
}

#[tokio::test]
async fn fallback_iterations_keep_models_and_serving_usage_without_summing_refusals() {
    // Cover a pre-output handoff, a mid-output handoff and sticky routing.
    for (mid_output, sticky, refused, partial_usage) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (true, false, true, false),
        (true, false, false, true),
    ] {
        let server = MockServer::start().await;
        let serving = "claude-opus-4-8";
        let initial_model = if mid_output {
            "claude-fable-5"
        } else {
            serving
        };
        let mut iterations = vec![];
        if !sticky {
            iterations.push(json!({"type": "message", "model": "claude-fable-5",
                "input_tokens": u64::MAX, "output_tokens": if mid_output { 10 } else { 0 },
                "cache_creation_input_tokens": 30, "cache_read_input_tokens": 40}));
        }
        iterations.push(json!({"type": "fallback_message", "model": serving,
            "input_tokens": 412, "output_tokens": 264,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}));
        let mut body = event(json!({"type": "message_start", "message": {
            "id": "msg", "model": initial_model, "usage": {
                "input_tokens": 999, "output_tokens": 1,
                "cache_creation_input_tokens": 30, "cache_read_input_tokens": 40}}}));
        if !sticky {
            let mut index = 0;
            if mid_output {
                body += &event(
                    json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
                );
                body += &event(
                    json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": "Partial output"}}),
                );
                body += &event(json!({"type": "content_block_stop", "index": index}));
                index += 1;
            }
            body += &event(
                json!({"type": "content_block_start", "index": index, "content_block": {
                "type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": serving}}}),
            );
            body += &event(json!({"type": "content_block_stop", "index": index}));
        }
        let final_usage = if partial_usage {
            json!({"output_tokens": 264, "iterations": iterations})
        } else {
            json!({"input_tokens": 412, "output_tokens": 264,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                "iterations": iterations})
        };
        body += &finish(final_usage, if refused { "refusal" } else { "end_turn" });
        mock_stream(&server, body).await;
        let response = collect(
            provider(&server.uri())
                .stream(request(), CancellationToken::new())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.model, serving);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 412,
                output_tokens: 264,
                ..Usage::default()
            }
        );
        assert_eq!(
            serde_json::to_value(&response.usage_iterations).unwrap(),
            json!(iterations)
        );
        assert_eq!(
            response.stop_reason,
            if refused {
                StopReason::Refusal
            } else {
                StopReason::EndTurn
            }
        );
    }
}

#[tokio::test]
async fn compaction_iterations_without_model_remain_separate_from_reply_usage() {
    let server = MockServer::start().await;
    let iteration = json!({"type": "compaction", "input_tokens": 144, "output_tokens": 276,
        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    mock_stream(
        &server,
        start()
            + &finish(
                json!({"input_tokens": 0, "output_tokens": 0,
        "iterations": [iteration]}),
                "compaction",
            ),
    )
    .await;
    let response = collect(
        provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.usage, Usage::default());
    assert_eq!(response.model, "claude-haiku-4-5");
    assert_eq!(
        serde_json::to_value(&response.usage_iterations).unwrap(),
        json!([iteration])
    );
}

#[tokio::test]
async fn stream_error_is_retryable_redacted_and_terminal() {
    let server = MockServer::start().await;
    mock_stream(&server, start() + &event(json!({"type": "error", "error": {"type": "overloaded_error", "message": format!("busy {KEY}")}})) + TEXT).await;
    let mut stream = provider(&server.uri())
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        stream.next().await.unwrap(),
        Ok(StreamEvent::MessageStart { .. })
    ));
    let error = stream.next().await.unwrap().unwrap_err();
    assert!(matches!(error, ProviderError::Stream { ref kind, .. } if kind == "overloaded_error"));
    assert!(error.is_retryable());
    assert!(!error.to_string().contains(KEY));
    assert!(stream.next().await.is_none());
}

async fn http_error(
    status: u16,
    message: &str,
    retry_header: Option<(&str, &str)>,
) -> ProviderError {
    let server = MockServer::start().await;
    let mut response = ResponseTemplate::new(status).set_body_json(
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}}),
    );
    if let Some((name, value)) = retry_header {
        response = response.insert_header(name, value);
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    match provider(&server.uri())
        .stream(request(), CancellationToken::new())
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("expected HTTP failure"),
    }
}

#[tokio::test]
async fn rate_limit_retry_after_seconds_and_milliseconds() {
    for (header, value, expected) in [
        ("retry-after", "2", 2000),
        ("retry-after", "0.5", 500),
        ("retry-after-ms", "125", 125),
    ] {
        let error = http_error(429, "slow down", Some((header, value))).await;
        assert!(matches!(error, ProviderError::Http { status: 429, .. }));
        assert!(error.is_retryable());
        assert_eq!(error.retry_after(), Some(Duration::from_millis(expected)));
    }
    assert_eq!(
        http_error(429, "slow down", Some(("retry-after", "-1")))
            .await
            .retry_after(),
        None
    );
}

#[tokio::test]
async fn bad_request_and_context_too_large() {
    let error = http_error(400, &format!("bad field {KEY}"), None).await;
    assert!(
        matches!(error, ProviderError::Http { status: 400, ref message, .. } if message == "invalid_request_error: bad field [redacted]")
    );
    for status in [400, 413] {
        assert!(matches!(
            http_error(status, "Prompt is too long: 250000 tokens", None).await,
            ProviderError::ContextTooLarge(_)
        ));
    }
    assert!(matches!(
        http_error(413, "request body too large", None).await,
        ProviderError::Http { status: 413, .. }
    ));
}

#[tokio::test]
async fn model_info_success_and_process_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/custom-model"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id": "custom-model", "max_input_tokens": 123456, "max_tokens": 7890}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let first = provider(&server.uri())
        .model_info("custom-model")
        .await
        .unwrap();
    let second = provider(&server.uri())
        .model_info("custom-model")
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.context_window, Some(123456));
    assert_eq!(first.max_output_tokens, Some(7890));
}

#[tokio::test]
async fn model_info_fallback_failures_are_not_cached() {
    let server = MockServer::start().await;
    let provider = provider(&server.uri());
    for (model, context, output) in [
        ("claude-opus-5-5", Some(1_000_000), Some(128_000)),
        ("claude-sonnet-5-5", Some(1_000_000), Some(128_000)),
        ("claude-fable-5-1", Some(1_000_000), Some(128_000)),
        ("claude-opus-5", Some(1_000_000), Some(128_000)),
        ("claude-sonnet-5", Some(1_000_000), Some(128_000)),
        ("claude-haiku-4-5", Some(200_000), Some(64_000)),
        ("unknown", None, None),
    ] {
        let info = provider.model_info(model).await.unwrap();
        assert_eq!(info.id, model);
        assert_eq!(info.context_window, context);
        assert_eq!(info.max_output_tokens, output);
    }
    Mock::given(method("GET"))
        .and(path("/v1/models/claude-haiku-4-5"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"max_input_tokens": 333, "max_tokens": 44})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        provider
            .model_info("claude-haiku-4-5")
            .await
            .unwrap()
            .context_window,
        Some(333)
    );
}

#[tokio::test]
async fn capability_and_fallback_limits_can_be_overridden() {
    let server = MockServer::start().await;
    mock_stream(&server, TEXT.into()).await;
    let mut cfg = config(&server.uri());
    cfg.model_caps.insert(
        "claude-opus-5-5".into(),
        ModelCaps {
            adaptive_thinking: false,
            context_window: Some(8),
            max_output_tokens: Some(4),
        },
    );
    let provider = AnthropicProvider::new(cfg).unwrap();
    assert_eq!(
        provider
            .model_info("claude-opus-5-5")
            .await
            .unwrap()
            .context_window,
        Some(8)
    );
    let mut req = request();
    req.model = "claude-opus-5-5".into();
    collect(
        provider
            .stream(req, CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    let posted = requests.iter().find(|req| req.method == "POST").unwrap();
    let body: Value = serde_json::from_slice(&posted.body).unwrap();
    assert!(body.get("thinking").is_none());
}

// A server that waits for the client to close. The guard aborts its task if
// an assertion panics, so these tests cannot leave a stalled server behind.
struct StallServer {
    url: String,
    task: Option<JoinHandle<()>>,
    ready: tokio::sync::oneshot::Receiver<()>,
}

impl Drop for StallServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl StallServer {
    async fn start(initial_body: Option<String>) -> Self {
        Self::script(initial_body, Vec::new()).await
    }

    async fn script(initial_body: Option<String>, frames: Vec<(Duration, String)>) -> Self {
        Self::http_script(initial_body, frames, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".into(), false).await
    }

    async fn http_script(
        initial_body: Option<String>,
        frames: Vec<(Duration, String)>,
        headers: String,
        truncate: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (ready_tx, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = ready_tx.send(());
            if let Some(body) = initial_body {
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket
                    .write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes())
                    .await
                    .unwrap();
            }
            for (delay, frame) in frames {
                tokio::time::sleep(delay).await;
                if socket
                    .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if truncate {
                return;
            }
            // Treat a reset as closure too. No more bytes are expected after
            // the complete request body has been drained.
            let read = socket.read(&mut buffer).await;
            assert!(
                matches!(read, Ok(0) | Err(_)),
                "client did not close the connection"
            );
        });
        Self {
            url,
            task: Some(task),
            ready,
        }
    }

    async fn closed(&mut self) {
        let task = self.task.as_mut().unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("connection did not close promptly")
            .unwrap();
        self.task.take();
    }
}

#[tokio::test]
async fn error_status_and_retry_after_survive_truncated_or_stalled_bodies() {
    for status in [429, 529] {
        for (truncate, idle_timeout) in [(true, false), (false, false), (false, true)] {
            let headers = format!(
                "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nretry-after: 7\r\nconnection: close\r\n\r\n"
            );
            let mut server =
                StallServer::http_script(Some("{\"error\":".into()), vec![], headers, truncate)
                    .await;
            let mut cfg = config(&server.url);
            if idle_timeout {
                cfg.idle_timeout = Duration::from_millis(200);
                cfg.request_timeout = Duration::from_secs(2);
            } else {
                cfg.request_timeout = Duration::from_millis(200);
            }
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                AnthropicProvider::new(cfg)
                    .unwrap()
                    .stream(request(), CancellationToken::new()),
            )
            .await
            .unwrap()
            .err()
            .unwrap();
            assert!(
                matches!(error, ProviderError::Http { status: actual, .. } if actual == status)
            );
            assert_eq!(error.charge(), AttemptCharge::Zero);
            assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
            assert!(error.is_retryable());
            let kind = if status == 429 {
                "rate_limit_error"
            } else {
                "overloaded_error"
            };
            assert!(error.to_string().contains(kind));
            server.closed().await;
        }
    }
}

#[tokio::test]
async fn idle_timeout_closes_connection_before_yielding_error() {
    let mut server = StallServer::start(Some(start())).await;
    let mut cfg = config(&server.url);
    cfg.idle_timeout = Duration::from_millis(200);
    let mut stream = AnthropicProvider::new(cfg)
        .unwrap()
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        stream.next().await.unwrap(),
        Ok(StreamEvent::MessageStart { .. })
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap(),
        Err(ProviderError::IdleTimeout)
    ));
    server.closed().await;
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn cancellation_mid_stream_closes_connection() {
    let mut server = StallServer::start(Some(start())).await;
    let cancel = CancellationToken::new();
    let mut stream = provider(&server.url)
        .stream(request(), cancel.clone())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap(),
        Err(ProviderError::Cancelled)
    ));
    server.closed().await;
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn pings_reset_idle_timeout() {
    let mut frames = vec![(Duration::from_millis(80), event(json!({"type": "ping"}))); 6];
    frames.push((
        Duration::from_millis(80),
        finish(json!({"output_tokens": 2}), "end_turn"),
    ));
    let mut server = StallServer::script(Some(start()), frames).await;
    let mut cfg = config(&server.url);
    cfg.idle_timeout = Duration::from_millis(200);
    let response = collect(
        AnthropicProvider::new(cfg)
            .unwrap()
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.usage.output_tokens, 2);
    server.closed().await;
}

#[tokio::test]
async fn partial_data_and_comments_do_not_reset_idle_timeout() {
    let frames = vec![(Duration::from_millis(50), ": keepalive\ndata: ".into()); 10];
    let mut server = StallServer::script(Some(start()), frames).await;
    let mut cfg = config(&server.url);
    cfg.idle_timeout = Duration::from_millis(200);
    let error = collect(
        AnthropicProvider::new(cfg)
            .unwrap()
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::IdleTimeout));
    server.closed().await;
}

#[tokio::test]
async fn cancellation_while_waiting_for_headers() {
    let mut server = StallServer::start(None).await;
    let provider = provider(&server.url);
    let cancel = CancellationToken::new();
    let sending = provider.stream(request(), cancel.clone());
    tokio::pin!(sending);
    tokio::select! {
        ready = &mut server.ready => ready.unwrap(),
        _ = &mut sending => panic!("server should stall before headers"),
    }
    cancel.cancel();
    let error = sending.await.err().unwrap();
    assert!(matches!(error, ProviderError::Cancelled));
    assert_eq!(error.charge(), AttemptCharge::Reserved);
    server.closed().await;
}

#[tokio::test]
async fn dropping_stream_closes_connection() {
    let mut server = StallServer::start(Some(start())).await;
    let mut stream = provider(&server.url)
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);
    server.closed().await;
}

#[tokio::test]
async fn whole_attempt_timeout_during_stream() {
    let mut server = StallServer::start(Some(start())).await;
    let mut cfg = config(&server.url);
    cfg.request_timeout = Duration::from_millis(200);
    let mut stream = AnthropicProvider::new(cfg)
        .unwrap()
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(
        matches!(stream.next().await.unwrap(), Err(ProviderError::Transport(message)) if message == "request timeout")
    );
    server.closed().await;
}

#[tokio::test]
async fn whole_attempt_timeout_before_headers() {
    let mut server = StallServer::start(None).await;
    let mut cfg = config(&server.url);
    cfg.request_timeout = Duration::from_millis(200);
    match AnthropicProvider::new(cfg)
        .unwrap()
        .stream(request(), CancellationToken::new())
        .await
    {
        Err(ProviderError::Transport(message)) => assert_eq!(message, "request timeout"),
        _ => panic!("expected whole attempt timeout"),
    }
    server.closed().await;
}

#[tokio::test]
async fn cancellation_before_dispatch_does_not_send() {
    let server = MockServer::start().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = provider(&server.uri())
        .stream(request(), cancel)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ProviderError::NotSent(_)));
    assert_eq!(error.charge(), AttemptCharge::Zero);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn refused_connection_is_not_sent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(matches!(
        provider(&url)
            .stream(request(), CancellationToken::new())
            .await,
        Err(ProviderError::NotSent(_))
    ));
}

#[tokio::test]
async fn malformed_json_truncated_stream_and_invalid_iterations_are_protocol_errors() {
    for body in [
        start(),
        "event: message_start\ndata: invalid\n\n".into(),
        start() + &finish(json!({"iterations": "invalid"}), "end_turn"),
    ] {
        let server = MockServer::start().await;
        mock_stream(&server, body).await;
        assert!(matches!(
            collect(
                provider(&server.uri())
                    .stream(request(), CancellationToken::new())
                    .await
                    .unwrap()
            )
            .await,
            Err(ProviderError::Protocol(_))
        ));
    }
}

#[tokio::test]
#[ignore = "requires KYORA_LIVE_TESTS=1 and ANTHROPIC_API_KEY"]
async fn live_smoke_only_with_explicit_opt_in() {
    if std::env::var("KYORA_LIVE_TESTS").as_deref() != Ok("1")
        || std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .is_none_or(|key| key.is_empty())
    {
        return;
    }
    let provider = AnthropicProvider::new(AnthropicConfig::from_env().unwrap()).unwrap();
    let response = collect(
        provider
            .stream(request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(!response.content.is_empty());
}
