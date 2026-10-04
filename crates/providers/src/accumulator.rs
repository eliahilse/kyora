use std::collections::{BTreeMap, btree_map::Entry};

use futures::StreamExt;
use kyora_protocol::{
    BlockStart, ContentBlock, ModelResponse, StopReason, StreamEvent, Usage, UsageIteration,
};
use serde_json::Value;

use crate::{EventStream, ProviderError};

struct PendingBlock {
    content: ContentBlock,
    partial_json: String,
    closed: bool,
}

/// Builds one response from streaming events, ordering content by block index.
/// Malformed tool JSON is preserved as a string and reported separately.
#[derive(Default)]
pub struct Accumulator {
    header: Option<(Option<String>, String)>,
    blocks: BTreeMap<usize, PendingBlock>,
    usage: Usage,
    usage_iterations: Vec<UsageIteration>,
    stop_reason: Option<StopReason>,
    stopped: bool,
    invalid_tool_inputs: Vec<usize>,
}

impl Accumulator {
    /// Creates an empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns indices of tool-use blocks whose JSON failed strict parsing.
    /// Check this after block stops and before executing any tools.
    pub fn invalid_tool_inputs(&self) -> &[usize] {
        &self.invalid_tool_inputs
    }

    /// Consumes one event, rejecting inconsistent stream structure.
    /// Non-zero message-delta counters replace the previous cumulative values.
    pub fn push(&mut self, event: StreamEvent) -> Result<(), ProviderError> {
        if self.stopped {
            return Err(protocol("event after message stop"));
        }
        if self.header.is_none() && !matches!(event, StreamEvent::MessageStart { .. }) {
            return Err(protocol("event before message start"));
        }
        match event {
            StreamEvent::MessageStart { id, model, usage } => {
                if self.header.is_some() {
                    return Err(protocol("duplicate message start"));
                }
                self.header = Some((id, model));
                self.usage = usage;
            }
            StreamEvent::BlockStart { index, block } => {
                let content = match block {
                    BlockStart::Text => ContentBlock::Text {
                        text: String::new(),
                    },
                    BlockStart::Thinking => ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: None,
                    },
                    BlockStart::ToolUse { id, name } => ContentBlock::ToolUse {
                        id,
                        name,
                        input: Value::Null,
                    },
                    BlockStart::Opaque {
                        provider,
                        kind,
                        raw,
                    } => ContentBlock::Opaque {
                        provider,
                        kind,
                        raw,
                    },
                };
                match self.blocks.entry(index) {
                    Entry::Vacant(entry) => {
                        entry.insert(PendingBlock {
                            content,
                            partial_json: String::new(),
                            closed: false,
                        });
                    }
                    Entry::Occupied(_) => {
                        return Err(protocol(format!("duplicate block index {index}")));
                    }
                }
            }
            StreamEvent::TextDelta { index, text } => match &mut self.open_block(index)?.content {
                ContentBlock::Text { text: existing } => existing.push_str(&text),
                _ => return Err(protocol(format!("text delta for non-text block {index}"))),
            },
            StreamEvent::ThinkingDelta { index, thinking } => {
                match &mut self.open_block(index)?.content {
                    ContentBlock::Thinking {
                        thinking: existing, ..
                    } => existing.push_str(&thinking),
                    _ => {
                        return Err(protocol(format!(
                            "thinking delta for non-thinking block {index}"
                        )));
                    }
                }
            }
            StreamEvent::SignatureDelta { index, signature } => {
                match &mut self.open_block(index)?.content {
                    ContentBlock::Thinking {
                        signature: existing,
                        ..
                    } => existing.get_or_insert_default().push_str(&signature),
                    _ => {
                        return Err(protocol(format!(
                            "signature delta for non-thinking block {index}"
                        )));
                    }
                }
            }
            StreamEvent::ToolInputDelta {
                index,
                partial_json,
            } => {
                let block = self.open_block(index)?;
                if !matches!(block.content, ContentBlock::ToolUse { .. }) {
                    return Err(protocol(format!(
                        "tool input delta for non-tool block {index}"
                    )));
                }
                block.partial_json.push_str(&partial_json);
            }
            StreamEvent::BlockStop { index } => {
                let block = self.open_block(index)?;
                let mut invalid = false;
                if let ContentBlock::ToolUse { input, .. } = &mut block.content {
                    *input = if block.partial_json.is_empty() {
                        serde_json::json!({})
                    } else {
                        match serde_json::from_str(&block.partial_json) {
                            Ok(value) => value,
                            Err(_) => {
                                invalid = true;
                                Value::String(block.partial_json.clone())
                            }
                        }
                    };
                }
                block.closed = true;
                if invalid {
                    self.invalid_tool_inputs.push(index);
                }
            }
            StreamEvent::MessageDelta { stop_reason, usage } => {
                if stop_reason.is_some() {
                    self.stop_reason = stop_reason;
                }
                for (current, latest) in [
                    (&mut self.usage.input_tokens, usage.input_tokens),
                    (&mut self.usage.output_tokens, usage.output_tokens),
                    (
                        &mut self.usage.cache_creation_input_tokens,
                        usage.cache_creation_input_tokens,
                    ),
                    (
                        &mut self.usage.cache_read_input_tokens,
                        usage.cache_read_input_tokens,
                    ),
                ] {
                    if latest != 0 {
                        *current = latest;
                    }
                }
            }
            StreamEvent::UsageSnapshot {
                model,
                usage,
                iterations,
            } => {
                self.header.as_mut().expect("message start was checked").1 = model;
                self.usage = usage;
                self.usage_iterations = iterations;
            }
            StreamEvent::MessageStop => {
                if self.blocks.values().any(|block| !block.closed) {
                    return Err(protocol("message stopped with open blocks"));
                }
                self.stopped = true;
            }
        }
        Ok(())
    }

    /// Finishes a complete stream, retaining malformed tool arguments as strings.
    /// Inspect [`Self::invalid_tool_inputs`] before consuming this accumulator.
    pub fn finish(self) -> Result<ModelResponse, ProviderError> {
        if !self.stopped {
            return Err(protocol("stream ended before message stop"));
        }
        let (id, model) = self
            .header
            .ok_or_else(|| protocol("missing message start"))?;
        Ok(ModelResponse {
            id,
            model,
            content: self
                .blocks
                .into_values()
                .map(|block| block.content)
                .collect(),
            stop_reason: self
                .stop_reason
                .ok_or_else(|| protocol("missing stop reason"))?,
            usage: self.usage,
            usage_iterations: self.usage_iterations,
        })
    }

    fn open_block(&mut self, index: usize) -> Result<&mut PendingBlock, ProviderError> {
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| protocol(format!("unknown block index {index}")))?;
        if block.closed {
            return Err(protocol(format!("block {index} is already stopped")));
        }
        Ok(block)
    }
}

fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::Protocol(message.into())
}

/// Collects a complete stream, propagating provider and protocol errors.
/// To inspect malformed tool-input indices, use [`Accumulator`] directly.
pub async fn collect(mut stream: EventStream) -> Result<ModelResponse, ProviderError> {
    let mut accumulator = Accumulator::new();
    while let Some(event) = stream.next().await {
        accumulator.push(event?)?;
    }
    accumulator.finish()
}
