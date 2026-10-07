use kyora_core::{
    Limits, ModelRef,
    tool::{truncate, validate},
};
use serde_json::json;
#[test]
fn schema_subset_rejects_wrong_missing_and_unknown_properties() {
    let schema = json!({"type":"object","required":["s"],"additionalProperties":false,"properties":{"s":{"type":"string"},"n":{"type":"integer"},"nested":{"type":"array","items":{"type":"boolean"}}}});
    assert!(validate(&schema, &json!({"s":"ok","n":2,"nested":[true,false]})).is_ok());
    for bad in [
        json!({}),
        json!({"s":1}),
        json!({"s":"ok","n":1.5}),
        json!({"s":"ok","x":true}),
        json!({"s":"ok","nested":[1]}),
    ] {
        assert!(validate(&schema, &bad).is_err());
    }
}
#[test]
fn truncation_bounds_unicode_and_reports_exact_omission() {
    let text = "α".repeat(300);
    let output = truncate(&text, 100);
    assert_eq!(output.chars().count(), 100);
    assert!(output.starts_with('α'));
    assert!(output.ends_with('α'));
    let count = output
        .split("[... ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(count, 300 - output.chars().filter(|c| *c == 'α').count());
    assert_eq!(truncate("abcdef", 3), "abc");
    assert_eq!(truncate("abc", 3), "abc");
    assert_eq!(truncate("abc", 0), "");
}
#[test]
fn defaults_validate_and_model_refs_parse() {
    let limits = Limits::default();
    limits.validate().unwrap();
    assert_eq!(limits.max_turns, 200);
    assert_eq!(limits.budget_tokens, 20_000_000);
    assert!(
        Limits {
            budget_tokens: 0,
            ..limits.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            run_timeout: std::time::Duration::ZERO,
            ..limits.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            max_depth: 0,
            ..limits
        }
        .validate()
        .is_err()
    );
    assert_eq!(
        "test".parse::<ModelRef>().unwrap().to_string(),
        "anthropic/test"
    );
    assert_eq!("fake/test".parse::<ModelRef>().unwrap().provider, "fake");
    for bad in ["", "/test", "fake/", " test"] {
        assert!(bad.parse::<ModelRef>().is_err());
    }
}

/// A tool whose schema uses constructs outside the local subset.
struct Remote(bool);

#[async_trait::async_trait]
impl kyora_core::Tool for Remote {
    fn spec(&self) -> kyora_protocol::ToolSpec {
        kyora_protocol::ToolSpec {
            name: if self.0 { "local" } else { "remote" }.into(),
            description: String::new(),
            input_schema: json!({"type":"object","properties":{"v":{"type":"null"}},"additionalProperties":false}),
            large_input: false,
        }
    }
    fn effect(&self) -> kyora_core::Effect {
        kyora_core::Effect::ReadOnly
    }
    fn validate_locally(&self) -> bool {
        self.0
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _cx: kyora_core::ToolCx,
    ) -> kyora_core::ToolOutput {
        kyora_core::ToolOutput::text("called")
    }
}

#[test]
fn tools_can_leave_input_validation_to_their_backend() {
    let tools = kyora_core::Toolset::new(vec![
        std::sync::Arc::new(Remote(true)),
        std::sync::Arc::new(Remote(false)),
    ])
    .unwrap();
    let input = json!({"v": null, "p_extra": 1});
    assert!(tools.validate("local", &input).is_err());
    assert!(tools.validate("remote", &input).is_ok());
    assert!(tools.validate("missing", &input).is_err());
}
