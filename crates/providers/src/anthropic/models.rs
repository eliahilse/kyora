use std::collections::HashMap;

use kyora_protocol::ModelInfo;

/// Capabilities and fallback limits for one model or model prefix.
/// Unknown models default to no adaptive thinking and unknown limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCaps {
    /// Whether to send an adaptive thinking configuration.
    pub adaptive_thinking: bool,
    /// Fallback context window, used when model discovery fails.
    pub context_window: Option<u64>,
    /// Fallback maximum output tokens, used when model discovery fails.
    pub max_output_tokens: Option<u32>,
}

/// Built-in capability defaults, keyed by model identifier prefix.
///
/// Opus/Sonnet/Fable/Mythos 5 and Opus 4.6/4.7/4.8 and Sonnet 4.6
/// use adaptive thinking. The known 5-series fallback limits are one million
/// input tokens and 128,000 output tokens; Haiku 4.5 has 200,000 and 64,000.
/// Replace or extend entries in [`super::AnthropicConfig::model_caps`] to override.
pub fn default_model_caps() -> HashMap<String, ModelCaps> {
    let mut table = HashMap::new();
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
        table.insert(
            prefix.into(),
            ModelCaps {
                adaptive_thinking: true,
                ..ModelCaps::default()
            },
        );
    }
    for prefix in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5-1"] {
        table.insert(
            prefix.into(),
            ModelCaps {
                adaptive_thinking: true,
                context_window: Some(1_000_000),
                max_output_tokens: Some(128_000),
            },
        );
    }
    table.insert(
        "claude-haiku-4-5".into(),
        ModelCaps {
            adaptive_thinking: false,
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
        },
    );
    table
}

pub(super) fn resolve(table: &HashMap<String, ModelCaps>, model: &str) -> ModelCaps {
    table
        .iter()
        .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, caps)| *caps)
        .unwrap_or_default()
}

impl ModelCaps {
    pub(super) fn info(self, model: &str) -> ModelInfo {
        ModelInfo {
            id: model.into(),
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
        }
    }
}
