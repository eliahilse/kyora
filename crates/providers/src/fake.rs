//! In-memory providers with conversation-local scripted queues and chunked events.

use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Mutex,
};

use async_trait::async_trait;
use futures::stream;
use kyora_protocol::{
    BlockStart, ContentBlock, ModelRequest, ModelResponse, Role, StreamEvent, Usage,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{EventStream, ModelProvider, ProviderError};

const DEFAULT_CHUNK_SIZE: usize = 16;

/// Optional request filters; all supplied filters must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matcher {
    /// Match a specific recursion depth.
    pub depth: Option<u32>,
    /// Match a substring of the last user message's visible text.
    pub last_user_contains: Option<String>,
    /// Match a substring of the system prompt; an absent prompt cannot match.
    pub system_contains: Option<String>,
}

impl Matcher {
    fn matches(&self, request: &ModelRequest) -> bool {
        self.depth
            .is_none_or(|depth| depth == request.metadata.depth)
            && self.last_user_contains.as_ref().is_none_or(|needle| {
                last_user_text(request).is_some_and(|text| text.contains(needle))
            })
            && self.system_contains.as_ref().is_none_or(|needle| {
                request
                    .system
                    .as_ref()
                    .is_some_and(|text| text.contains(needle))
            })
    }
}

/// An ordered rule with a response queue copied separately for each conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Optional filters, serialized as `match` in a fixture script.
    #[serde(default, rename = "match")]
    pub matcher: Option<Matcher>,
    /// Responses consumed in order within each conversation.
    pub responses: Vec<ModelResponse>,
}

/// A deterministic provider using the first matching, non-exhausted rule.
/// Conversation keys hash the system prompt and complete first message, so
/// different initial tasks consume independent copies of every rule's queue.
pub struct ScriptedProvider {
    rules: Vec<Rule>,
    queues: Mutex<HashMap<(usize, u64), VecDeque<ModelResponse>>>,
    chunk_size: usize,
}

impl ScriptedProvider {
    /// Creates a provider with ordered rules and default 16-byte chunks.
    pub fn new(rules: Vec<Rule>) -> Self {
        Self {
            rules,
            queues: Mutex::new(HashMap::new()),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Loads `{"rules": [{"match": {...}, "responses": [...]}]}` fixture JSON.
    /// Response IDs, models, and usage may be omitted.
    pub fn from_json(json: &str) -> Result<Self, ProviderError> {
        #[derive(Deserialize)]
        struct Script {
            rules: Vec<Rule>,
        }
        let script: Script = serde_json::from_str(json)
            .map_err(|error| ProviderError::Protocol(format!("invalid fake script: {error}")))?;
        Ok(Self::new(script.rules))
    }

    /// Sets a maximum chunk size in bytes, except that a UTF-8 character is never
    /// split and may exceed that size. Zero is normalized to one byte.
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1);
        self
    }

    fn next_response(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let mut hasher = DefaultHasher::new();
        request.system.hash(&mut hasher);
        serde_json::to_vec(&request.messages.first())
            .map_err(|error| ProviderError::Protocol(error.to_string()))?
            .hash(&mut hasher);
        let key = hasher.finish();
        let mut queues = self
            .queues
            .lock()
            .map_err(|_| ProviderError::Other("fake provider: queue lock poisoned".into()))?;
        for (index, rule) in self.rules.iter().enumerate() {
            if rule
                .matcher
                .as_ref()
                .is_some_and(|matcher| !matcher.matches(request))
            {
                continue;
            }
            let queue = queues
                .entry((index, key))
                .or_insert_with(|| rule.responses.clone().into());
            if let Some(response) = queue.pop_front() {
                return Ok(response);
            }
        }
        Err(ProviderError::Other(format!(
            "fake provider: no scripted response for depth {} and last user text {:?}",
            request.metadata.depth,
            last_user_text(request).unwrap_or_default(),
        )))
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "fake"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        response_stream(self.next_response(&req)?, &req, self.chunk_size, cancel)
    }
}

/// An ad hoc provider backed by a synchronous, thread-safe response closure.
pub struct FnProvider<F> {
    respond: F,
    chunk_size: usize,
}

impl<F> FnProvider<F>
where
    F: Fn(&ModelRequest) -> Result<ModelResponse, ProviderError> + Send + Sync,
{
    /// Wraps a closure, emitting its responses as a realistic event stream.
    pub fn new(respond: F) -> Self {
        Self {
            respond,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Configures UTF-8-safe byte chunks, normalizing zero to one byte.
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1);
        self
    }
}

#[async_trait]
impl<F> ModelProvider for FnProvider<F>
where
    F: Fn(&ModelRequest) -> Result<ModelResponse, ProviderError> + Send + Sync,
{
    fn name(&self) -> &str {
        "fake"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        response_stream((self.respond)(&req)?, &req, self.chunk_size, cancel)
    }
}

fn last_user_text(request: &ModelRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.text())
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if index > start && index + character.len_utf8() - start > size {
            chunks.push(text[start..index].to_owned());
            start = index;
        }
    }
    chunks.push(text[start..].to_owned());
    chunks
}

fn response_stream(
    mut response: ModelResponse,
    request: &ModelRequest,
    chunk_size: usize,
    cancel: CancellationToken,
) -> Result<EventStream, ProviderError> {
    if response.model.is_empty() {
        response.model.clone_from(&request.model);
    }
    if response.usage == Usage::default() {
        let input_bytes = request.system.as_ref().map_or(0, String::len)
            + request
                .messages
                .iter()
                .map(|message| message.text().len())
                .sum::<usize>();
        let output_bytes = response
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.len(),
                _ => 0,
            })
            .sum::<usize>();
        response.usage = Usage {
            input_tokens: (input_bytes as u64 / 4).max(1),
            output_tokens: (output_bytes as u64 / 4).max(1),
            ..Usage::default()
        };
    }
    let mut events = vec![StreamEvent::MessageStart {
        id: response.id,
        model: response.model,
        usage: Usage {
            output_tokens: 0,
            ..response.usage
        },
    }];
    for (index, content) in response.content.into_iter().enumerate() {
        let (block, deltas) = match content {
            ContentBlock::Text { text } => (
                BlockStart::Text,
                chunks(&text, chunk_size)
                    .into_iter()
                    .map(|text| StreamEvent::TextDelta { index, text })
                    .collect::<Vec<_>>(),
            ),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                let mut deltas = chunks(&thinking, chunk_size)
                    .into_iter()
                    .map(|thinking| StreamEvent::ThinkingDelta { index, thinking })
                    .collect::<Vec<_>>();
                if let Some(signature) = signature {
                    deltas.extend(
                        chunks(&signature, chunk_size)
                            .into_iter()
                            .map(|signature| StreamEvent::SignatureDelta { index, signature }),
                    );
                }
                (BlockStart::Thinking, deltas)
            }
            ContentBlock::ToolUse { id, name, input } => {
                let json = serde_json::to_string(&input)
                    .map_err(|error| ProviderError::Protocol(error.to_string()))?;
                (
                    BlockStart::ToolUse { id, name },
                    chunks(&json, chunk_size)
                        .into_iter()
                        .map(|partial_json| StreamEvent::ToolInputDelta {
                            index,
                            partial_json,
                        })
                        .collect(),
                )
            }
            ContentBlock::Opaque {
                provider,
                kind,
                raw,
            } => (
                BlockStart::Opaque {
                    provider,
                    kind,
                    raw,
                },
                Vec::new(),
            ),
            ContentBlock::ToolResult { .. } => {
                return Err(ProviderError::Protocol(
                    "tool results cannot be streamed as model output".into(),
                ));
            }
        };
        events.push(StreamEvent::BlockStart { index, block });
        events.extend(deltas);
        events.push(StreamEvent::BlockStop { index });
    }
    events.push(StreamEvent::MessageDelta {
        stop_reason: Some(response.stop_reason),
        usage: Usage {
            output_tokens: response.usage.output_tokens,
            ..Usage::default()
        },
    });
    events.push(StreamEvent::MessageStop);
    Ok(Box::pin(stream::unfold(
        (events.into_iter(), cancel, false),
        |(mut events, cancel, done)| async move {
            if done {
                return None;
            }
            if cancel.is_cancelled() {
                return Some((Err(ProviderError::Cancelled), (events, cancel, true)));
            }
            events
                .next()
                .map(|event| (Ok(event), (events, cancel, false)))
        },
    )))
}
