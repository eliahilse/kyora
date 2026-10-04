use kyora_protocol::{
    ContentBlock, Effort, Message, ModelRequest, Role, ThinkingDisplay, ToolResultPart, ToolSpec,
};
use kyora_providers::anthropic::{AnthropicConfig, ModelCaps, build_request, default_model_caps};
use serde_json::json;

fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-haiku-4-5".into(),
        max_tokens: 64,
        messages: vec![Message::user_text("Hello")],
        ..ModelRequest::default()
    }
}

fn config() -> AnthropicConfig {
    AnthropicConfig::new("test-only-credential")
}

#[test]
fn plain_text_golden_never_sends_metadata() {
    let mut req = request();
    req.metadata.node_id = Some("private-node".into());
    req.metadata.depth = 42;
    let (body, betas) = build_request(&req, &ModelCaps::default(), &config());
    assert_eq!(
        body,
        json!({
            "model": "claude-haiku-4-5", "max_tokens": 64, "stream": true,
            "cache_control": {"type": "ephemeral"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]
        })
    );
    assert!(betas.is_empty());
}

#[test]
fn system_and_disable_cache_golden() {
    let mut req = request();
    req.system = Some("System".into());
    let (mut expected, _) = build_request(&request(), &ModelCaps::default(), &config());
    expected["system"] =
        json!([{"type": "text", "text": "System", "cache_control": {"type": "ephemeral"}}]);
    assert_eq!(
        build_request(&req, &ModelCaps::default(), &config()).0,
        expected
    );
    req.options.disable_cache = true;
    expected.as_object_mut().unwrap().remove("cache_control");
    expected["system"][0]
        .as_object_mut()
        .unwrap()
        .remove("cache_control");
    assert_eq!(
        build_request(&req, &ModelCaps::default(), &config()).0,
        expected
    );
    req.system = None;
    assert!(
        build_request(&req, &ModelCaps::default(), &config())
            .0
            .get("system")
            .is_none()
    );
}

#[test]
fn tools_golden() {
    let mut req = request();
    req.tools = [false, true]
        .into_iter()
        .map(|large_input| ToolSpec {
            name: "write".into(),
            description: "Write a file".into(),
            input_schema: json!({"type": "object"}),
            large_input,
        })
        .collect();
    let (mut expected, _) = build_request(&request(), &ModelCaps::default(), &config());
    expected["tools"] = json!([
        {"name": "write", "description": "Write a file", "input_schema": {"type": "object"}},
        {"name": "write", "description": "Write a file", "input_schema": {"type": "object"}, "eager_input_streaming": true}
    ]);
    assert_eq!(
        build_request(&req, &ModelCaps::default(), &config()).0,
        expected
    );
}

#[test]
fn history_golden_preserves_signed_and_opaque_blocks() {
    let mut req = request();
    let opaque = json!({"type": "redacted_thinking", "data": "opaque-bytes", "future": [1, 2]});
    let foreign = ContentBlock::Opaque {
        provider: "other".into(),
        kind: "foreign".into(),
        raw: json!({"private": true}),
    };
    req.messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "unverified".into(),
                    signature: None,
                },
                ContentBlock::Thinking {
                    thinking: "verbatim\n".into(),
                    signature: Some("signed".into()),
                },
                ContentBlock::Thinking {
                    thinking: "empty signature allowed".into(),
                    signature: Some(String::new()),
                },
                ContentBlock::Opaque {
                    provider: "anthropic".into(),
                    kind: "redacted_thinking".into(),
                    raw: opaque.clone(),
                },
                foreign.clone(),
                ContentBlock::ToolUse {
                    id: "tool_1".into(),
                    name: "write".into(),
                    input: json!({"code": "print(1)"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![foreign],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "skip".into(),
                signature: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".into(),
                content: vec![
                    ToolResultPart::Text {
                        text: "failed".into(),
                    },
                    ToolResultPart::Text {
                        text: "details".into(),
                    },
                ],
                is_error: true,
            }],
        },
    ];
    let (mut expected, _) = build_request(&request(), &ModelCaps::default(), &config());
    expected["messages"] = json!([
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "verbatim\n", "signature": "signed"},
            {"type": "thinking", "thinking": "empty signature allowed", "signature": ""},
            opaque,
            {"type": "tool_use", "id": "tool_1", "name": "write", "input": {"code": "print(1)"}}
        ]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": [
            {"type": "text", "text": "failed"}, {"type": "text", "text": "details"}
        ], "is_error": true}]}
    ]);
    assert_eq!(
        build_request(&req, &ModelCaps::default(), &config()).0,
        expected
    );
}

#[test]
fn adaptive_thinking_display_and_binding_golden() {
    let mut req = request();
    let caps = ModelCaps {
        adaptive_thinking: true,
        ..ModelCaps::default()
    };
    let mut cfg = config();
    let (mut expected, _) = build_request(&req, &ModelCaps::default(), &cfg);
    expected["thinking"] =
        json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "error"}});
    assert_eq!(
        build_request(&req, &caps, &cfg),
        (
            expected.clone(),
            vec!["thinking-binding-controls-2026-08-01".into()]
        )
    );
    for (display, name) in [
        (ThinkingDisplay::Omitted, "omitted"),
        (ThinkingDisplay::Summarized, "summarized"),
    ] {
        req.options.thinking_display = Some(display);
        expected["thinking"]["display"] = json!(name);
        assert_eq!(build_request(&req, &caps, &cfg).0, expected);
    }
    cfg.prefix_mismatch_behavior = "drop_block".into();
    expected["thinking"]["block_binding"]["prefix_mismatch_behavior"] = json!("drop_block");
    assert_eq!(build_request(&req, &caps, &cfg).0, expected);
    cfg.enable_block_binding = false;
    expected["thinking"]
        .as_object_mut()
        .unwrap()
        .remove("block_binding");
    assert_eq!(build_request(&req, &caps, &cfg), (expected, vec![]));
    assert!(
        build_request(&req, &ModelCaps::default(), &cfg)
            .0
            .get("thinking")
            .is_none()
    );
}

#[test]
fn effort_golden() {
    for (effort, name) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::Xhigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let mut req = request();
        req.options.effort = Some(effort);
        let (mut expected, _) = build_request(&request(), &ModelCaps::default(), &config());
        expected["output_config"] = json!({"effort": name});
        assert_eq!(
            build_request(&req, &ModelCaps::default(), &config()).0,
            expected
        );
    }
}

#[test]
fn task_budget_threshold_and_beta_composition_golden() {
    let mut req = request();
    req.options.task_budget_total = Some(19_999);
    let cfg = config();
    assert_eq!(
        build_request(&req, &ModelCaps::default(), &cfg),
        build_request(&request(), &ModelCaps::default(), &cfg)
    );
    for total in [20_000, 45_000] {
        req.options.task_budget_total = Some(total);
        req.options.effort = Some(Effort::High);
        let mut cfg = config();
        cfg.extra_betas = vec![
            "task-budgets-2026-03-13".into(),
            "custom".into(),
            "thinking-binding-controls-2026-08-01".into(),
            "custom".into(),
            "last".into(),
        ];
        let caps = ModelCaps {
            adaptive_thinking: true,
            ..ModelCaps::default()
        };
        let (mut expected, _) = build_request(&request(), &caps, &config());
        expected["output_config"] =
            json!({"effort": "high", "task_budget": {"type": "tokens", "total": total}});
        assert_eq!(
            build_request(&req, &caps, &cfg),
            (
                expected,
                vec![
                    "thinking-binding-controls-2026-08-01".into(),
                    "task-budgets-2026-03-13".into(),
                    "custom".into(),
                    "last".into()
                ]
            )
        );
    }
}

#[test]
fn defaults_and_debug_redaction() {
    let mut cfg = config();
    assert_eq!(cfg.idle_timeout.as_secs(), 300);
    assert_eq!(cfg.request_timeout.as_secs(), 1800);
    assert_eq!(cfg.connect_timeout.as_secs(), 30);
    assert_eq!(cfg.base_url, "https://api.anthropic.com");
    cfg.extra_betas.push(cfg.api_key.clone());
    assert!(!format!("{cfg:?}").contains(&cfg.api_key));
    let table = default_model_caps();
    for prefix in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
    ] {
        assert!(table[prefix].adaptive_thinking);
    }
    assert!(!table["claude-haiku-4-5"].adaptive_thinking);
}

#[test]
fn from_env_reads_required_key_and_optional_url() {
    // Use child processes to avoid mutating the shared test process environment.
    // std::env::set_var is unsafe in Rust 2024 and is not needed here.
    for mode in ["missing", "empty", "default", "override"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "from_env_child", "--nocapture"])
            .env("KYORA_CONFIG_TEST_MODE", mode)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_BASE_URL");
        if mode == "empty" {
            command.env("ANTHROPIC_API_KEY", "");
        }
        if matches!(mode, "default" | "override") {
            command.env("ANTHROPIC_API_KEY", "test-only-credential");
        }
        if mode == "override" {
            command.env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1234/proxy");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mode {mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn from_env_child() {
    let Ok(mode) = std::env::var("KYORA_CONFIG_TEST_MODE") else {
        return;
    };
    match mode.as_str() {
        "missing" | "empty" => assert!(AnthropicConfig::from_env().is_err()),
        "default" | "override" => {
            let cfg = AnthropicConfig::from_env().unwrap();
            assert_eq!(cfg.api_key, "test-only-credential");
            assert_eq!(
                cfg.base_url,
                if mode == "override" {
                    "http://127.0.0.1:1234/proxy"
                } else {
                    "https://api.anthropic.com"
                }
            );
        }
        _ => panic!("unexpected config test mode"),
    }
}
