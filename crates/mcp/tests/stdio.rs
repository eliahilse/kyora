#![cfg(unix)]
use kyora_core::{
    AgentSpec, Effect, Limits, Runtime, RuntimeConfig, Status, Tool, ToolCx, ToolOutput,
    ToolSelection, Toolset, TraceEvent, TraceSink,
};
use kyora_mcp::{McpConfig, McpToolsets, Server, ServerConfig, Servers};
use kyora_protocol::{ContentBlock, ModelRequest, ModelResponse, StopReason, ToolSpec, Usage};
use kyora_providers::{ModelProvider, RetryPolicy, fake::FnProvider};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const SERVER: &str = env!("CARGO_BIN_EXE_kyora-mcp-test-server");

fn server_config(log: &Path, settings: &[(&str, &str)]) -> ServerConfig {
    let mut env = BTreeMap::from([("KYORA_MCP_TEST_LOG".to_owned(), log.display().to_string())]);
    env.extend(
        settings
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    ServerConfig {
        command: Some(SERVER.into()),
        env,
        ..ServerConfig::default()
    }
}

fn environment() -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = std::env::vars_os().collect();
    for (name, value) in [
        ("ANTHROPIC_API_KEY", "test-only-must-not-leak"),
        ("KYORA_TEST_FORWARD", "forwarded"),
        ("KYORA_TEST_UNLISTED", "dropped"),
    ] {
        env.push((name.into(), value.into()));
    }
    env
}

async fn start(dir: &Path, config: &ServerConfig) -> Arc<Server> {
    Server::start("fake", config, dir, &environment())
        .await
        .unwrap()
}

fn messages(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        // A record still being written is picked up by the next poll.
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Polls the server log until `matches` holds for some message.
async fn logged(log: &Path, matches: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(message) = messages(log).into_iter().find(&matches) {
            return message;
        }
        assert!(Instant::now() < deadline, "not logged: {:?}", messages(log));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn call_id(log: &Path, tool: &str) -> Value {
    messages(log)
        .into_iter()
        .rev()
        .find(|m| m["method"] == "tools/call" && m["params"]["name"] == tool)
        .map(|m| m["id"].clone())
        .expect("call logged")
}

async fn cancelled_notice(log: &Path, id: &Value) -> Value {
    logged(log, |m| {
        m["method"] == "notifications/cancelled" && m["params"]["requestId"] == *id
    })
    .await
}

fn pid(log: &Path) -> u32 {
    messages(log)[0]["pid"].as_u64().unwrap() as u32
}

/// True once the process is gone or only a zombie awaiting its reaper.
fn gone(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

async fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !gone(pid) {
        assert!(Instant::now() < deadline, "process {pid} still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Stands in for a built-in tool in combined toolsets.
struct Builtin;

#[async_trait::async_trait]
impl Tool for Builtin {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "builtin".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            large_input: false,
        }
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _input: Value, _cx: ToolCx) -> ToolOutput {
        ToolOutput::text("builtin")
    }
}

fn find(server: &Server, name: &str) -> Arc<dyn Tool> {
    server
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}

#[tokio::test]
async fn handshake_pages_tools_and_calls_them() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let server = start(dir.path(), &server_config(&log, &[])).await;
    let log_messages = messages(&log);
    let initialize = &log_messages[1];
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(initialize["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(initialize["params"]["clientInfo"]["name"], "kyora");
    let capabilities = &initialize["params"]["capabilities"];
    for offered in ["sampling", "roots", "elicitation"] {
        assert!(capabilities.get(offered).is_none(), "{capabilities}");
    }
    assert_eq!(log_messages[2]["method"], "notifications/initialized");
    let pages: Vec<_> = log_messages
        .iter()
        .filter(|m| m["method"] == "tools/list")
        .map(|m| m["params"]["cursor"].clone())
        .collect();
    assert_eq!(
        pages,
        [
            Value::Null,
            json!("2"),
            json!("4"),
            json!("6"),
            json!("8"),
            json!("10")
        ]
    );
    let names: Vec<_> = server.tools().iter().map(|tool| tool.spec().name).collect();
    assert_eq!(names.len(), 12);
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("mcp__fake__dotted_name_"))
    );
    let echo = find(&server, "mcp__fake__echo").spec();
    assert_eq!(echo.description, "Echo text.");
    assert_eq!(echo.input_schema["required"], json!(["text"]));
    assert_eq!(find(&server, "mcp__fake__echo").effect(), Effect::Mutating);
    assert_eq!(find(&server, "mcp__fake__look").effect(), Effect::ReadOnly);
    assert_eq!(find(&server, "mcp__fake__look").spec().description, "Look");

    let cancel = CancellationToken::new();
    let echoed = server.call("echo", json!({"text": "hello"}), &cancel).await;
    assert!(!echoed.is_error);
    assert_eq!(echoed.text_content(), "hello");
    let failed = server.call("fail", json!({}), &cancel).await;
    assert!(failed.is_error);
    assert_eq!(failed.text_content(), "boom");
    let mixed = server.call("mixed", json!({}), &cancel).await;
    assert!(mixed.text_content().contains("[image: image/png, 5 bytes]"));
    assert!(
        mixed
            .text_content()
            .contains("[resource: file:///notes.txt]\nalpha")
    );
    let structured = server.call("structured", json!({}), &cancel).await;
    assert_eq!(structured.text_content(), r#"{"answer":42}"#);
    let unknown = server.call("missing", json!({}), &cancel).await;
    assert!(unknown.is_error);
    assert_eq!(unknown.text_content(), "error -32602: unknown tool");
    assert_eq!(
        server
            .call("dotted.name", json!({}), &cancel)
            .await
            .text_content(),
        "looked"
    );
    server.shutdown().await;
    assert!(
        server
            .call("echo", json!({"text": "x"}), &cancel)
            .await
            .is_error
    );
}

#[tokio::test]
async fn timeouts_and_cancellation_notify_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let mut config = server_config(&log, &[]);
    config.tool_timeout_s = Some(0.3);
    let server = start(dir.path(), &config).await;
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let output = server.call("slow", json!({}), &cancel).await;
    assert!(output.is_error);
    assert!(
        output.text_content().starts_with("timed out"),
        "{}",
        output.text_content()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let notice = cancelled_notice(&log, &call_id(&log, "slow")).await;
    assert_eq!(notice["params"]["reason"], "timed out");
    server.shutdown().await;

    config.tool_timeout_s = None;
    let log = dir.path().join("second.jsonl");
    config
        .env
        .insert("KYORA_MCP_TEST_LOG".into(), log.display().to_string());
    let server = start(dir.path(), &config).await;
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let output = server.call("slow", json!({}), &cancel).await;
    assert!(output.is_error);
    assert_eq!(output.text_content(), "cancelled");
    let notice = cancelled_notice(&log, &call_id(&log, "slow")).await;
    assert_eq!(notice["params"]["reason"], "cancelled");
    // The connection stays usable after a cancelled call.
    let fresh = CancellationToken::new();
    let echoed = server
        .call("echo", json!({"text": "still here"}), &fresh)
        .await;
    assert_eq!(echoed.text_content(), "still here");
    server.shutdown().await;
}

#[tokio::test]
async fn a_server_crashing_mid_call_becomes_a_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let server = start(dir.path(), &server_config(&log, &[])).await;
    let cancel = CancellationToken::new();
    let output = server.call("crash", json!({}), &cancel).await;
    assert!(output.is_error);
    assert!(
        output.text_content().contains("closed the connection"),
        "{}",
        output.text_content()
    );
    assert!(
        server
            .call("echo", json!({"text": "x"}), &cancel)
            .await
            .is_error
    );
    let started = Instant::now();
    server.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    wait_gone(pid(&log)).await;
}

#[tokio::test]
async fn allow_and_deny_lists_filter_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = server_config(&dir.path().join("log.jsonl"), &[]);
    config.allow_tools = Some(vec!["echo".into(), "fail".into(), "look".into()]);
    config.deny_tools = vec!["fail".into()];
    let server = start(dir.path(), &config).await;
    let names: Vec<_> = server.tools().iter().map(|tool| tool.spec().name).collect();
    assert_eq!(names, ["mcp__fake__echo", "mcp__fake__look"]);
    server.shutdown().await;
}

#[tokio::test]
async fn environment_and_working_directory_are_controlled() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut config = server_config(&dir.path().join("log.jsonl"), &[("MODE", "fast")]);
    config.env_vars = vec!["KYORA_TEST_FORWARD".into()];
    config.cwd = Some("sub".into());
    let server = start(dir.path(), &config).await;
    let names = [
        "MODE",
        "KYORA_TEST_FORWARD",
        "KYORA_TEST_UNLISTED",
        "ANTHROPIC_API_KEY",
        "PATH",
    ];
    let output = server
        .call("env", json!({"names": names}), &CancellationToken::new())
        .await;
    let report: Value = serde_json::from_str(&output.text_content()).unwrap();
    let vars = report["vars"].as_object().unwrap();
    let mut seen: Vec<_> = vars.keys().map(String::as_str).collect();
    seen.sort_unstable();
    assert_eq!(seen, ["KYORA_TEST_FORWARD", "MODE", "PATH"]);
    assert_eq!(vars["KYORA_TEST_FORWARD"], "forwarded");
    assert_eq!(
        PathBuf::from(report["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir.path().join("sub").canonicalize().unwrap()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_leaves_no_processes_behind() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let server = start(dir.path(), &server_config(&log, &[])).await;
    let spawned = server
        .call("spawn", json!({}), &CancellationToken::new())
        .await;
    let child: u32 = spawned.text_content().parse().unwrap();
    assert!(!gone(child));
    server.shutdown().await;
    wait_gone(pid(&log)).await;
    wait_gone(child).await;
}

#[tokio::test]
async fn shutdown_escalates_to_signals_when_a_server_ignores_end_of_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let server = start(
        dir.path(),
        &server_config(&log, &[("KYORA_MCP_TEST_MODE", "linger")]),
    )
    .await;
    let started = Instant::now();
    server.shutdown().await;
    let elapsed = started.elapsed();
    assert!(elapsed >= kyora_mcp::defaults::EXIT_GRACE, "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    wait_gone(pid(&log)).await;
}

#[tokio::test]
async fn startup_failures_are_reported_and_other_servers_keep_running() {
    let dir = tempfile::tempdir().unwrap();
    let log = |name: &str| dir.path().join(format!("{name}.jsonl"));
    let mut hangs = server_config(&log("hangs"), &[("KYORA_MCP_TEST_MODE", "hang")]);
    hangs.startup_timeout_s = Some(0.5);
    let mut disabled = server_config(&log("disabled"), &[("KYORA_MCP_TEST_MODE", "exit")]);
    disabled.enabled = false;
    let config = McpConfig {
        servers: BTreeMap::from([
            ("good".into(), server_config(&log("good"), &[])),
            (
                "missing".into(),
                ServerConfig {
                    command: Some("/nonexistent/kyora-mcp-missing".into()),
                    ..ServerConfig::default()
                },
            ),
            (
                "exits".into(),
                server_config(&log("exits"), &[("KYORA_MCP_TEST_MODE", "exit")]),
            ),
            ("hangs".into(), hangs),
            (
                "old".into(),
                server_config(&log("old"), &[("KYORA_MCP_TEST_MODE", "bad-version")]),
            ),
            (
                "bare".into(),
                server_config(&log("bare"), &[("KYORA_MCP_TEST_MODE", "no-tools")]),
            ),
            ("disabled".into(), disabled),
        ]),
    };
    let started = Instant::now();
    let (servers, failures) = Servers::start_with_env(&config, dir.path(), &environment()).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let running: Vec<_> = servers.servers().iter().map(|s| s.name()).collect();
    assert_eq!(running, ["bare", "good"]);
    assert!(servers.servers()[0].tools().is_empty());
    let failures: Vec<_> = failures.iter().map(|e| format!("{e:#}")).collect();
    assert_eq!(failures.len(), 4, "{failures:?}");
    let failure = |name: &str| {
        failures
            .iter()
            .find(|f| f.starts_with(&format!("mcp server {name}: ")))
            .unwrap_or_else(|| panic!("no failure for {name}: {failures:?}"))
    };
    assert!(failure("missing").contains("spawn"));
    assert!(
        failure("exits").contains("refusing to start"),
        "{}",
        failure("exits")
    );
    assert!(failure("hangs").contains("timed out"));
    assert!(failure("old").contains("unsupported protocol version 1999-01-01"));
    assert!(!log("disabled").exists());
    for name in ["exits", "hangs", "old"] {
        wait_gone(pid(&log(name))).await;
    }
    assert_eq!(servers.tools().len(), 12);
    servers.shutdown().await;
    wait_gone(pid(&log("good"))).await;
}

#[tokio::test]
async fn list_changed_refreshes_the_tools_later_nodes_receive() {
    let dir = tempfile::tempdir().unwrap();
    let config = McpConfig {
        servers: BTreeMap::from([(
            "fake".into(),
            server_config(&dir.path().join("log.jsonl"), &[]),
        )]),
    };
    let (servers, failures) = Servers::start_with_env(&config, dir.path(), &environment()).await;
    assert!(failures.is_empty());
    let servers = Arc::new(servers);
    let base = Toolset::new(vec![Arc::new(Builtin)]).unwrap();
    let toolsets = McpToolsets::new(&base, servers.clone());
    let snapshot = toolsets.snapshot().unwrap();
    assert!(snapshot.get("builtin").is_some());
    assert!(snapshot.get("mcp__fake__echo").is_some());
    assert!(snapshot.get("mcp__fake__added").is_none());
    let selected = snapshot
        .select(&ToolSelection(Some(vec![
            "builtin".into(),
            "mcp__fake__look".into(),
        ])))
        .unwrap();
    assert_eq!(selected.specs().len(), 2);
    let output = servers.servers()[0]
        .call("notify", json!({}), &CancellationToken::new())
        .await;
    assert_eq!(output.text_content(), "ok");
    let deadline = Instant::now() + Duration::from_secs(10);
    while toolsets
        .snapshot()
        .unwrap()
        .get("mcp__fake__added")
        .is_none()
    {
        assert!(Instant::now() < deadline, "tool list was not refreshed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    servers.shutdown().await;
}

fn scripted(toolsets: McpToolsets, tool: &str, input: Value) -> (Runtime, TraceSink) {
    let tool = tool.to_owned();
    let provider = FnProvider::new(move |request: &ModelRequest| {
        let first = request.messages.len() == 1;
        Ok(ModelResponse {
            id: None,
            model: String::new(),
            content: if first {
                vec![ContentBlock::ToolUse {
                    id: "call".into(),
                    name: tool.clone(),
                    input: input.clone(),
                }]
            } else {
                vec![ContentBlock::Text {
                    text: "done".into(),
                }]
            },
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: Usage::default(),
            usage_iterations: vec![],
        })
    });
    let trace = TraceSink::ephemeral();
    let runtime = Runtime::new(RuntimeConfig {
        providers: BTreeMap::from([("fake".into(), Arc::new(provider) as Arc<dyn ModelProvider>)]),
        toolsets: Arc::new(toolsets),
        limits: Limits::default(),
        retry: RetryPolicy::default(),
        llm_model: "fake/leaf".parse().unwrap(),
        trace: trace.clone(),
        session: "test".into(),
    })
    .unwrap();
    (runtime, trace)
}

fn spec(dir: &Path, tools: Option<Vec<String>>) -> AgentSpec {
    let mut spec = AgentSpec::new("task", dir.into());
    spec.model = "fake/test".parse().unwrap();
    spec.tools = ToolSelection(tools);
    spec
}

#[tokio::test]
async fn the_runtime_calls_mcp_tools_like_any_other_tool() {
    let dir = tempfile::tempdir().unwrap();
    let config = McpConfig {
        servers: BTreeMap::from([(
            "fake".into(),
            server_config(&dir.path().join("log.jsonl"), &[]),
        )]),
    };
    let (servers, _) = Servers::start_with_env(&config, dir.path(), &environment()).await;
    let servers = Arc::new(servers);
    let (runtime, trace) = scripted(
        McpToolsets::new(&Toolset::default(), servers.clone()),
        "mcp__fake__echo",
        json!({"text": "through the runtime"}),
    );
    let mut events = trace.subscribe();
    let outcome = runtime
        .run(spec(dir.path(), Some(vec!["mcp__fake__echo".into()])))
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let mut offered = None;
    let mut result = None;
    while let Ok(record) = events.try_recv() {
        match record.event {
            TraceEvent::NodeStart { tools, .. } => offered = Some(tools),
            TraceEvent::ToolResult {
                content, is_error, ..
            } => result = Some((content, is_error)),
            _ => {}
        }
    }
    let offered: Vec<_> = offered.unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(offered, ["mcp__fake__echo"]);
    assert_eq!(result.unwrap(), ("through the runtime".into(), false));
    servers.shutdown().await;
}

#[tokio::test]
async fn runtime_cancellation_reaches_read_only_and_mutating_calls() {
    for tool in ["slow_read", "slow"] {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let config = McpConfig {
            servers: BTreeMap::from([("fake".into(), server_config(&log, &[]))]),
        };
        let (servers, _) = Servers::start_with_env(&config, dir.path(), &environment()).await;
        let servers = Arc::new(servers);
        let (runtime, trace) = scripted(
            McpToolsets::new(&Toolset::default(), servers.clone()),
            &format!("mcp__fake__{tool}"),
            json!({}),
        );
        let mut events = trace.subscribe();
        let control = runtime.clone();
        let watched = log.clone();
        let name = tool.to_owned();
        tokio::spawn(async move {
            logged(&watched, |m| {
                m["method"] == "tools/call" && m["params"]["name"] == name.as_str()
            })
            .await;
            control.cancel();
        });
        let outcome = runtime.run(spec(dir.path(), None)).await.unwrap();
        assert_eq!(outcome.status, Status::Cancelled, "{tool}");
        let mut result = None;
        while let Ok(record) = events.try_recv() {
            if let TraceEvent::ToolResult { content, .. } = record.event {
                result = Some(content);
            }
        }
        assert_eq!(result.as_deref(), Some("cancelled"), "{tool}");
        let notice = cancelled_notice(&log, &call_id(&log, tool)).await;
        assert_eq!(notice["params"]["reason"], "cancelled", "{tool}");
        servers.shutdown().await;
    }
}
