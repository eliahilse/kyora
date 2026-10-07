//! Scripted MCP server for the kyora-mcp tests: newline-delimited JSON-RPC on stdio.
//!
//! KYORA_MCP_TEST_LOG appends every received message (and the pid) as JSON lines.
//! KYORA_MCP_TEST_MODE selects a startup failure (exit, spill, hang, bad-version,
//! no-tools, flood, many, bulky), linger, which keeps running after stdin closes, or stall-list,
//! which stops answering tools/list once the notify tool has run.
//! KYORA_MCP_TEST_PAGE sets the tools/list page size (default 2).
//! KYORA_MCP_TEST_EXTRA_TOOL adds a tool with that name, which echoes its arguments;
//! KYORA_MCP_TEST_EXTRA_SCHEMA and KYORA_MCP_TEST_EXTRA_DESCRIPTION set its input schema
//! and description.
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

type Shared<T> = Arc<Mutex<T>>;

fn main() {
    let mode = std::env::var("KYORA_MCP_TEST_MODE").unwrap_or_default();
    let log: Option<Shared<std::fs::File>> = std::env::var_os("KYORA_MCP_TEST_LOG").map(|path| {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open log");
        Arc::new(Mutex::new(file))
    });
    let record = move |value: &Value| {
        if let Some(log) = &log {
            // One write per record, so readers never see half a line.
            let line = format!("{value}\n");
            log.lock()
                .unwrap()
                .write_all(line.as_bytes())
                .expect("write log");
        }
    };
    record(&json!({"pid": std::process::id()}));
    eprintln!("test server starting");
    if mode == "spill" {
        // A secret followed by just enough output to push its start out of a tail.
        let secret = std::env::var("KYORA_MCP_TEST_SECRET").unwrap_or_default();
        eprint!("{secret}{}", "y".repeat(2048 - 8));
        std::process::exit(2);
    }
    if mode == "exit" {
        let secret = std::env::var("KYORA_MCP_TEST_SECRET").unwrap_or_default();
        eprintln!("fatal: refusing to start with secret {secret}");
        std::process::exit(2);
    }
    let page: usize = std::env::var("KYORA_MCP_TEST_PAGE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    let out: Shared<std::io::Stdout> = Arc::new(Mutex::new(std::io::stdout()));
    let cancelled: Shared<HashSet<String>> = Arc::default();
    let added = Arc::new(AtomicBool::new(false));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        record(&message);
        let id = message["id"].clone();
        let params = &message["params"];
        match message["method"].as_str().unwrap_or_default() {
            "initialize" if mode == "hang" => {}
            "initialize" if mode == "flood" => {
                // One line longer than any message limit, never terminated.
                let mut out = out.lock().unwrap();
                let _ = out.write_all(&vec![b'a'; 17 * 1024 * 1024]);
                let _ = out.flush();
            }
            "initialize" => {
                let version = if mode == "bad-version" {
                    json!("1999-01-01")
                } else {
                    params["protocolVersion"].clone()
                };
                let capabilities = if mode == "no-tools" {
                    json!({})
                } else {
                    json!({"tools": {"listChanged": true}})
                };
                reply(
                    &out,
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": capabilities,
                        "serverInfo": {"name": "kyora-test", "version": "1"},
                    }),
                );
            }
            "notifications/cancelled" => {
                cancelled
                    .lock()
                    .unwrap()
                    .insert(params["requestId"].to_string());
            }
            "ping" => reply(&out, &id, json!({})),
            "tools/list" if mode == "stall-list" && added.load(Ordering::SeqCst) => {}
            "tools/list" => {
                let tools = tools(&mode, added.load(Ordering::SeqCst));
                let start: usize = params["cursor"]
                    .as_str()
                    .and_then(|cursor| cursor.parse().ok())
                    .unwrap_or(0);
                let end = (start + page).min(tools.len());
                let mut result = json!({"tools": tools[start..end]});
                if end < tools.len() {
                    result["nextCursor"] = json!(end.to_string());
                }
                reply(&out, &id, result);
            }
            "tools/call" => {
                let arguments = params["arguments"].clone();
                let extra = std::env::var("KYORA_MCP_TEST_EXTRA_TOOL").ok();
                if extra.as_deref() == params["name"].as_str() {
                    reply(&out, &id, text(&arguments.to_string()));
                    continue;
                }
                match params["name"].as_str().unwrap_or_default() {
                    "echo" => reply(&out, &id, text(arguments["text"].as_str().unwrap_or(""))),
                    "fail" => reply(
                        &out,
                        &id,
                        json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
                    ),
                    "mixed" => reply(&out, &id, mixed()),
                    "structured" => {
                        // Echoes non-empty arguments as structured content.
                        let structured = match arguments.as_object() {
                            Some(map) if !map.is_empty() => arguments.clone(),
                            _ => json!({"answer": 42}),
                        };
                        reply(
                            &out,
                            &id,
                            json!({"content": [], "structuredContent": structured}),
                        );
                    }
                    "look" | "dotted.name" => reply(&out, &id, text("looked")),
                    "slow" | "slow_read" => {
                        let out = out.clone();
                        let cancelled = cancelled.clone();
                        let millis = arguments["ms"].as_u64().unwrap_or(30_000);
                        std::thread::spawn(move || {
                            let until = Instant::now() + Duration::from_millis(millis);
                            while Instant::now() < until {
                                if cancelled.lock().unwrap().contains(&id.to_string()) {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            reply(&out, &id, text("done"));
                        });
                    }
                    "crash" => std::process::exit(3),
                    "env" => {
                        let mut vars = serde_json::Map::new();
                        for name in arguments["names"].as_array().into_iter().flatten() {
                            let name = name.as_str().unwrap_or_default();
                            if let Ok(value) = std::env::var(name) {
                                vars.insert(name.into(), json!(value));
                            }
                        }
                        let cwd = std::env::current_dir().unwrap();
                        let body = json!({"cwd": cwd, "vars": vars});
                        reply(&out, &id, text(&body.to_string()));
                    }
                    "spawn" => {
                        // Left running on purpose: shutdown must end it through the process group.
                        #[allow(clippy::zombie_processes)]
                        let child = std::process::Command::new("sleep")
                            .arg("600")
                            .spawn()
                            .expect("spawn sleep");
                        reply(&out, &id, text(&child.id().to_string()));
                    }
                    "notify" => {
                        added.store(true, Ordering::SeqCst);
                        send(
                            &out,
                            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                        );
                        reply(&out, &id, text("ok"));
                    }
                    "added" => reply(&out, &id, text("added")),
                    _ => send(
                        &out,
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown tool"}}),
                    ),
                }
            }
            method if !id.is_null() => send(
                &out,
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {method}")}}),
            ),
            _ => {}
        }
    }
    if mode == "linger" {
        std::thread::sleep(Duration::from_secs(600));
    }
}

fn tools(mode: &str, added: bool) -> Vec<Value> {
    let object = json!({"type": "object"});
    if mode == "many" {
        return (0..=1024)
            .map(|i| json!({"name": format!("t{i}"), "inputSchema": object}))
            .collect();
    }
    if mode == "bulky" {
        let description = "x".repeat(3 * 1024 * 1024);
        return (0..3)
            .map(|i| json!({"name": format!("t{i}"), "description": description, "inputSchema": object}))
            .collect();
    }
    let mut tools = vec![
        json!({"name": "echo", "description": "Echo text.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "fail", "inputSchema": object}),
        json!({"name": "mixed", "inputSchema": object}),
        json!({"name": "structured", "inputSchema": object}),
        json!({"name": "slow", "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}}),
        json!({"name": "slow_read", "inputSchema": object, "annotations": {"readOnlyHint": true}}),
        json!({"name": "crash", "inputSchema": object}),
        json!({"name": "look", "title": "Look", "inputSchema": object, "annotations": {"readOnlyHint": true}}),
        json!({"name": "env", "inputSchema": {"type": "object", "properties": {"names": {"type": "array", "items": {"type": "string"}}}}}),
        json!({"name": "spawn", "inputSchema": object}),
        json!({"name": "notify", "inputSchema": object}),
        json!({"name": "dotted.name", "inputSchema": object}),
    ];
    if added {
        tools.push(json!({"name": "added", "inputSchema": object}));
    }
    if let Ok(name) = std::env::var("KYORA_MCP_TEST_EXTRA_TOOL") {
        let schema = std::env::var("KYORA_MCP_TEST_EXTRA_SCHEMA")
            .ok()
            .and_then(|schema| serde_json::from_str(&schema).ok())
            .unwrap_or(object);
        let description = std::env::var("KYORA_MCP_TEST_EXTRA_DESCRIPTION").unwrap_or_default();
        tools.push(json!({"name": name, "description": description, "inputSchema": schema}));
    }
    tools
}

fn text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn mixed() -> Value {
    json!({"content": [
        {"type": "text", "text": "header"},
        {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
        {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
        {"type": "resource", "resource": {"uri": "file:///notes.txt", "mimeType": "text/plain", "text": "alpha"}},
        {"type": "resource", "resource": {"uri": "file:///blob.bin", "mimeType": "application/zip", "blob": "AAAAAA=="}},
        {"type": "resource_link", "uri": "file:///linked.md", "name": "linked", "description": "A linked file."},
    ]})
}

fn reply(out: &Shared<std::io::Stdout>, id: &Value, result: Value) {
    send(out, json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn send(out: &Shared<std::io::Stdout>, message: Value) {
    let mut out = out.lock().unwrap();
    writeln!(out, "{message}").expect("write stdout");
    out.flush().expect("flush stdout");
}
