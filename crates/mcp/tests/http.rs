#![cfg(unix)]
use kyora_mcp::{Server, ServerConfig};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "test-only-bearer-token";
const SESSION: &str = "session-1";

/// A streamable HTTP MCP server: JSON replies for some requests, SSE for others.
struct Fake;

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match request.method.as_str() {
            // No standalone server stream; sessions can be deleted.
            "GET" => return ResponseTemplate::new(405),
            "DELETE" => return ResponseTemplate::new(200),
            _ => {}
        }
        let message: Value = serde_json::from_slice(&request.body).unwrap();
        let id = message["id"].clone();
        let params = &message["params"];
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => json_reply(
                &id,
                json!({
                    "protocolVersion": params["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "remote", "version": "1"},
                }),
            )
            .insert_header("mcp-session-id", SESSION),
            "tools/list" if params["cursor"].is_null() => json_reply(
                &id,
                json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}], "nextCursor": "page-2"}),
            ),
            "tools/list" => sse_reply(
                &id,
                json!({"tools": [
                    {"name": "fail", "inputSchema": {"type": "object"}},
                    {"name": "slow", "inputSchema": {"type": "object"}},
                ]}),
            ),
            "tools/call" => match params["name"].as_str().unwrap_or_default() {
                "echo" => sse_reply(
                    &id,
                    json!({"content": [{"type": "text", "text": params["arguments"]["text"]}]}),
                ),
                "fail" => json_reply(
                    &id,
                    json!({"content": [{"type": "text", "text": "remote failure"}], "isError": true}),
                ),
                _ => sse_reply(&id, json!({"content": []})).set_delay(Duration::from_secs(30)),
            },
            _ if id.is_null() => ResponseTemplate::new(202),
            _ => json_reply(&id, json!({})),
        }
    }
}

fn json_reply(id: &Value, result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn sse_reply(id: &Value, result: Value) -> ResponseTemplate {
    let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
    ResponseTemplate::new(200).set_body_raw(
        format!("event: message\ndata: {message}\n\n"),
        "text/event-stream",
    )
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn method(request: &Request) -> String {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|message| message["method"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[tokio::test]
async fn streamable_http_handshake_paging_calls_and_cleanup() {
    let mock = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/mcp"))
        .respond_with(Fake)
        .mount(&mock)
        .await;
    let config = ServerConfig {
        url: Some(format!("{}/mcp", mock.uri())),
        bearer_token_env: Some("REMOTE_TOKEN".into()),
        headers: BTreeMap::from([("X-Team".into(), "core".into())]),
        env_headers: BTreeMap::from([("X-Api-Key".into(), "REMOTE_KEY".into())]),
        tool_timeout_s: Some(0.3),
        ..ServerConfig::default()
    };
    let env: Vec<(OsString, OsString)> = vec![
        ("REMOTE_TOKEN".into(), TOKEN.into()),
        ("REMOTE_KEY".into(), "test-only-header-key".into()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start("remote", &config, dir.path(), &env)
        .await
        .unwrap();
    let names: Vec<_> = server.tools().iter().map(|tool| tool.spec().name).collect();
    assert_eq!(
        names,
        [
            "mcp__remote__echo",
            "mcp__remote__fail",
            "mcp__remote__slow"
        ]
    );

    let cancel = CancellationToken::new();
    let echoed = server
        .call("echo", json!({"text": "over http"}), &cancel)
        .await;
    assert!(!echoed.is_error, "{}", echoed.text_content());
    assert_eq!(echoed.text_content(), "over http");
    let failed = server.call("fail", json!({}), &cancel).await;
    assert!(failed.is_error);
    assert_eq!(failed.text_content(), "remote failure");
    let started = Instant::now();
    let slow = server.call("slow", json!({}), &cancel).await;
    assert!(
        slow.text_content().starts_with("timed out"),
        "{}",
        slow.text_content()
    );
    assert!(started.elapsed() < Duration::from_secs(10));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let requests = mock.received_requests().await.unwrap();
        if requests
            .iter()
            .any(|request| method(request) == "notifications/cancelled")
        {
            break;
        }
        assert!(Instant::now() < deadline, "no cancellation notice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.shutdown().await;

    let requests = mock.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.method.as_str() == "DELETE"
                && header(request, "mcp-session-id") == Some(SESSION))
    );
    for request in &requests {
        assert_eq!(
            header(request, "authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(header(request, "x-team"), Some("core"));
        assert_eq!(header(request, "x-api-key"), Some("test-only-header-key"));
        if method(request) != "initialize" {
            assert_eq!(header(request, "mcp-session-id"), Some(SESSION));
            assert_eq!(header(request, "mcp-protocol-version"), Some("2025-11-25"));
        }
    }
}

#[tokio::test]
async fn http_failures_are_startup_errors() {
    let mock = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"))
        .mount(&mock)
        .await;
    let config = ServerConfig {
        url: Some(mock.uri()),
        bearer_token_env: Some("REMOTE_TOKEN".into()),
        ..ServerConfig::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let missing = Server::start("remote", &config, dir.path(), &[]).await;
    let message = format!("{:#}", missing.err().unwrap());
    assert!(message.contains("REMOTE_TOKEN is not set"), "{message}");
    let env: [(OsString, OsString); 1] = [("REMOTE_TOKEN".into(), TOKEN.into())];
    let refused = Server::start("remote", &config, dir.path(), &env).await;
    let message = format!("{:#}", refused.err().unwrap());
    assert!(
        message.contains("send initialize request: Auth required"),
        "{message}"
    );
    assert!(!message.contains("rmcp::"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
}
