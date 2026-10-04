//! Provider-neutral, IO-free messages and streaming events.

use std::ops::{Add, AddAssign};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The author of a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A user or tool-result message.
    User,
    /// A model-generated message.
    Assistant,
}

/// A conversation message with ordered content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The message author.
    pub role: Role,
    /// The message content in replay order.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Creates a user message containing one text block.
    pub fn user_text(s: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: s.into() }],
        }
    }

    /// Creates an assistant message containing one text block.
    pub fn assistant_text(s: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: s.into() }],
        }
    }

    /// Concatenates text blocks without separators, excluding reasoning and tools.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Iterates over tool-use IDs, names, and inputs in content order.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

/// One part of a message's content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Visible text.
    Text {
        /// The text content.
        text: String,
    },
    /// Provider reasoning, which must be replayed verbatim.
    Thinking {
        /// The reasoning content.
        thinking: String,
        /// The provider's optional reasoning signature.
        signature: Option<String>,
    },
    /// A request to execute a tool.
    ToolUse {
        /// The provider's tool-use identifier.
        id: String,
        /// The tool name.
        name: String,
        /// The parsed tool arguments.
        input: Value,
    },
    /// The result of a tool execution.
    ToolResult {
        /// The corresponding tool-use identifier.
        tool_use_id: String,
        /// The result content.
        content: Vec<ToolResultPart>,
        /// Whether the execution failed.
        is_error: bool,
    },
    /// A provider-specific block, replayed verbatim only to the same provider.
    /// Other providers must drop it.
    Opaque {
        /// The originating provider's name.
        provider: String,
        /// The provider-specific block kind.
        kind: String,
        /// The original provider data, including redacted reasoning or compaction.
        raw: Value,
    },
}

/// One part of a tool result; additional content kinds can be added later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    /// Text returned by a tool.
    Text {
        /// The result text.
        text: String,
    },
}

/// A tool definition made available to the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name.
    pub name: String,
    /// A description of the tool's behavior.
    pub description: String,
    /// The JSON schema for the tool's input.
    pub input_schema: Value,
    /// The tool takes large inputs (code, file contents), so providers should
    /// stream its input as generated instead of buffering it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub large_input: bool,
}

/// Token counters, with omitted JSON fields defaulting to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Input tokens written to the cache.
    pub cache_creation_input_tokens: u64,
    /// Input tokens read from the cache.
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Returns the sum of all four token counters.
    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

impl Add for Usage {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self {
        self += rhs;
        self
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cache_creation_input_tokens += rhs.cache_creation_input_tokens;
        self.cache_read_input_tokens += rhs.cache_read_input_tokens;
    }
}

/// Usage for one provider-reported attempt, kept separate across models.
/// Reported tokens can include unbilled refusals, so this is not a bill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageIteration {
    /// The model that ran this attempt.
    pub model: String,
    /// The provider's iteration kind, such as `message` or `fallback_message`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Token counters for this attempt only.
    #[serde(flatten)]
    pub usage: Usage,
}

/// Why a model stopped, preserving unknown provider strings verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The assistant finished its turn.
    EndTurn,
    /// The assistant requested tool execution.
    ToolUse,
    /// The output token limit was reached.
    MaxTokens,
    /// A configured stop sequence was reached.
    StopSequence,
    /// The provider paused the turn.
    PauseTurn,
    /// The provider refused the request.
    Refusal,
    /// The response filled the model's context window.
    ModelContextWindowExceeded,
    /// An unrecognized provider stop reason.
    Other(String),
}

impl Serialize for StopReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::PauseTurn => "pause_turn",
            Self::Refusal => "refusal",
            Self::ModelContextWindowExceeded => "model_context_window_exceeded",
            Self::Other(reason) => reason,
        })
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "pause_turn" => Self::PauseTurn,
            "refusal" => Self::Refusal,
            "model_context_window_exceeded" => Self::ModelContextWindowExceeded,
            reason => Self::Other(reason.to_owned()),
        })
    }
}

/// Internal request context for tracing and fake providers; never sent to APIs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestMeta {
    /// The optional runtime node identifier.
    pub node_id: Option<String>,
    /// The node's recursion depth.
    pub depth: u32,
}

/// Requested reasoning effort; providers map it to their own setting or ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Least reasoning.
    Low,
    /// Moderate reasoning.
    Medium,
    /// Thorough reasoning.
    High,
    /// More than high.
    Xhigh,
    /// Most reasoning.
    Max,
}

/// How provider reasoning is returned to the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    /// Reasoning blocks are returned without readable text.
    #[default]
    Omitted,
    /// Reasoning blocks carry a readable summary.
    Summarized,
}

/// Provider-neutral request options; each provider maps what it supports and ignores the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestOptions {
    /// Reasoning effort; `None` leaves the provider default.
    pub effort: Option<Effort>,
    /// Reasoning display; `None` leaves the provider default.
    pub thinking_display: Option<ThinkingDisplay>,
    /// Disables prompt-caching hints when true.
    pub disable_cache: bool,
    /// Advisory token budget for the whole agentic loop, sent where supported.
    pub task_budget_total: Option<u64>,
}

/// Model limits discovered from a provider; unknown values are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The model identifier the information describes.
    pub id: String,
    /// Maximum input tokens (the context window).
    pub context_window: Option<u64>,
    /// Maximum output tokens for one response.
    pub max_output_tokens: Option<u32>,
}

/// A provider-neutral model request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// The requested model identifier.
    pub model: String,
    /// An optional system prompt.
    pub system: Option<String>,
    /// The conversation in order.
    pub messages: Vec<Message>,
    /// Tools available for this request.
    pub tools: Vec<ToolSpec>,
    /// The maximum output token count.
    pub max_tokens: u32,
    /// Provider-neutral options.
    #[serde(default)]
    pub options: RequestOptions,
    /// Internal context that must not be sent to a real provider API.
    pub metadata: RequestMeta,
}

/// A complete model response; optional fixture fields default when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// The provider's optional response identifier.
    pub id: Option<String>,
    /// The model identifier; defaults to an empty string in fixture JSON.
    #[serde(default)]
    pub model: String,
    /// Generated content in order.
    pub content: Vec<ContentBlock>,
    /// The reason generation ended.
    pub stop_reason: StopReason,
    /// Token counters; defaults to zero in fixture JSON.
    #[serde(default)]
    pub usage: Usage,
}

/// The initial information for a streamed content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockStart {
    /// A visible text block.
    Text,
    /// A reasoning block.
    Thinking,
    /// A tool-use block whose arguments arrive as JSON deltas.
    ToolUse {
        /// The tool-use identifier.
        id: String,
        /// The tool name.
        name: String,
    },
    /// A provider-specific block transmitted intact.
    Opaque {
        /// The originating provider.
        provider: String,
        /// The block kind.
        kind: String,
        /// The original provider data.
        raw: Value,
    },
}

/// A provider-neutral streaming event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Begins a response with its initial usage counters.
    MessageStart {
        /// The optional response identifier.
        id: Option<String>,
        /// The model identifier.
        model: String,
        /// Initial token counters.
        usage: Usage,
    },
    /// Begins a block at an index in the final content array.
    BlockStart {
        /// The block index.
        index: usize,
        /// The initial block information.
        block: BlockStart,
    },
    /// Appends visible text to a block.
    TextDelta {
        /// The block index.
        index: usize,
        /// The text fragment.
        text: String,
    },
    /// Appends reasoning to a block.
    ThinkingDelta {
        /// The block index.
        index: usize,
        /// The reasoning fragment.
        thinking: String,
    },
    /// Appends a reasoning signature to a block.
    SignatureDelta {
        /// The block index.
        index: usize,
        /// The signature fragment.
        signature: String,
    },
    /// Appends raw JSON tool arguments to a block.
    ToolInputDelta {
        /// The block index.
        index: usize,
        /// The JSON fragment.
        partial_json: String,
    },
    /// Ends a block, allowing its tool arguments to be parsed.
    BlockStop {
        /// The block index.
        index: usize,
    },
    /// Updates the stop reason and cumulative usage counters.
    MessageDelta {
        /// A stop reason, if one is available.
        stop_reason: Option<StopReason>,
        /// Latest cumulative counters; zero fields mean no update.
        usage: Usage,
    },
    /// Ends the response.
    MessageStop,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_iteration_retains_model_and_kind() {
        let raw = json!({"model": "fallback-model", "type": "fallback_message",
            "input_tokens": 12, "output_tokens": 3,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
        let iteration: UsageIteration = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(iteration.model, "fallback-model");
        assert_eq!(iteration.kind, "fallback_message");
        assert_eq!(iteration.usage.input_tokens, 12);
        assert_eq!(serde_json::to_value(iteration).unwrap(), raw);
    }

    #[test]
    fn message_and_all_blocks_round_trip() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Thinking {
                thinking: "reason".into(),
                signature: Some("sig".into()),
            },
            ContentBlock::Thinking {
                thinking: "reason".into(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "echo".into(),
                input: json!({"a": 1}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ToolResultPart::Text { text: "ok".into() }],
                is_error: false,
            },
            ContentBlock::Opaque {
                provider: "test".into(),
                kind: "redacted_thinking".into(),
                raw: json!({"data": "opaque"}),
            },
        ];
        for block in &blocks {
            let value = serde_json::to_value(block).unwrap();
            assert!(value["type"].is_string());
            assert_eq!(
                serde_json::from_value::<ContentBlock>(value).unwrap(),
                *block
            );
        }
        for role in [Role::User, Role::Assistant] {
            let message = Message {
                role,
                content: blocks.clone(),
            };
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        }
    }

    #[test]
    fn message_helpers_filter_blocks() {
        let mut message = Message::user_text("hello");
        message.content.push(ContentBlock::Thinking {
            thinking: "hidden".into(),
            signature: None,
        });
        message.content.push(ContentBlock::ToolUse {
            id: "t".into(),
            name: "echo".into(),
            input: json!({}),
        });
        message.content.push(ContentBlock::Text {
            text: " world".into(),
        });
        assert_eq!(message.role, Role::User);
        assert_eq!(message.text(), "hello world");
        assert_eq!(
            message.tool_uses().collect::<Vec<_>>(),
            vec![("t", "echo", &json!({}))]
        );
        assert_eq!(Message::assistant_text("answer").role, Role::Assistant);
    }

    #[test]
    fn stop_reasons_are_strings_and_preserve_unknown_values() {
        for (reason, wire) in [
            (StopReason::EndTurn, "end_turn"),
            (StopReason::ToolUse, "tool_use"),
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::StopSequence, "stop_sequence"),
            (StopReason::PauseTurn, "pause_turn"),
            (StopReason::Refusal, "refusal"),
            (
                StopReason::ModelContextWindowExceeded,
                "model_context_window_exceeded",
            ),
            (StopReason::Other("future_reason".into()), "future_reason"),
            (StopReason::Other(String::new()), ""),
        ] {
            let value = serde_json::to_value(&reason).unwrap();
            assert_eq!(value, json!(wire));
            assert_eq!(serde_json::from_value::<StopReason>(value).unwrap(), reason);
        }
    }

    #[test]
    fn usage_arithmetic() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 4,
        };
        let expected = Usage {
            input_tokens: 2,
            output_tokens: 4,
            cache_creation_input_tokens: 6,
            cache_read_input_tokens: 8,
        };
        assert_eq!(Usage::default().total(), 0);
        assert_eq!(usage.total(), 10);
        assert_eq!(usage + usage, expected);
        let mut assigned = usage;
        assigned += usage;
        assert_eq!(assigned, expected);
        assert_eq!(
            serde_json::from_str::<Usage>("{\"output_tokens\":2}")
                .unwrap()
                .output_tokens,
            2
        );
    }
}
