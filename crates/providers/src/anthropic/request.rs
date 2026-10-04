use kyora_protocol::{ContentBlock, ModelRequest};
use serde_json::{Value, json};

use super::{AnthropicConfig, ModelCaps};

pub(super) const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
pub(super) const TASK_BETA: &str = "task-budgets-2026-03-13";
// The API's minimum accepted task budget, not a client default.
const MIN_TASK_BUDGET: u64 = 20_000;

/// Builds the Messages API body and stable, deduplicated beta list without IO.
/// Internal request metadata is never serialized. Capability overrides are
/// supplied explicitly through `caps`.
pub fn build_request(
    req: &ModelRequest,
    caps: &ModelCaps,
    cfg: &AnthropicConfig,
) -> (Value, Vec<String>) {
    let messages: Vec<_> = req
        .messages
        .iter()
        .filter_map(|message| {
            let content: Vec<_> = message.content.iter().filter_map(block).collect();
            (!content.is_empty()).then(|| json!({"role": message.role, "content": content}))
        })
        .collect();
    let mut body = json!({"model": req.model, "max_tokens": req.max_tokens, "stream": true, "messages": messages});
    let mut betas = Vec::new();
    if !req.options.disable_cache {
        body["cache_control"] = json!({"type": "ephemeral"});
    }
    if let Some(system) = &req.system {
        let mut text = json!({"type": "text", "text": system});
        if !req.options.disable_cache {
            text["cache_control"] = json!({"type": "ephemeral"});
        }
        body["system"] = json!([text]);
    }
    if !req.tools.is_empty() {
        body["tools"] = req.tools.iter().map(|tool| {
            let mut value = json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema});
            if tool.large_input { value["eager_input_streaming"] = json!(true); }
            value
        }).collect();
    }
    if caps.adaptive_thinking {
        let mut thinking = json!({"type": "adaptive"});
        if let Some(display) = req.options.thinking_display {
            thinking["display"] = json!(display);
        }
        if cfg.enable_block_binding {
            thinking["block_binding"] =
                json!({"prefix_mismatch_behavior": cfg.prefix_mismatch_behavior});
            betas.push(BINDING_BETA.to_owned());
        }
        body["thinking"] = thinking;
    }
    if let Some(effort) = req.options.effort {
        body["output_config"] = json!({"effort": effort});
    }
    if let Some(total) = req
        .options
        .task_budget_total
        .filter(|total| *total >= MIN_TASK_BUDGET)
    {
        if body.get("output_config").is_none() {
            body["output_config"] = json!({});
        }
        body["output_config"]["task_budget"] = json!({"type": "tokens", "total": total});
        betas.push(TASK_BETA.to_owned());
    }
    for beta in &cfg.extra_betas {
        if !betas.contains(beta) {
            betas.push(beta.clone());
        }
    }
    (body, betas)
}

fn block(block: &ContentBlock) -> Option<Value> {
    Some(match block {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::Thinking {
            thinking,
            signature: Some(signature),
        } => json!({"type": "thinking", "thinking": thinking, "signature": signature}),
        ContentBlock::Thinking {
            signature: None, ..
        } => return None,
        ContentBlock::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
        }
        ContentBlock::Opaque { provider, raw, .. } if provider == "anthropic" => raw.clone(),
        ContentBlock::Opaque { .. } => return None,
    })
}
