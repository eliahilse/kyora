use std::time::Duration;

use kyora_protocol::{BlockStart, StreamEvent, Usage};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    error,
    sse::{Event, Parser},
};
use crate::{EventStream, ProviderError};

pub(super) fn response_stream(
    response: reqwest::Response,
    cancel: CancellationToken,
    idle_timeout: Duration,
    deadline: Instant,
    key: String,
) -> EventStream {
    Box::pin(async_stream::stream! {
        let mut response = Some(response);
        let mut parser = Parser::new();
        let mut pending = std::collections::VecDeque::new();
        let mut idle_deadline = Instant::now() + idle_timeout;
        loop {
            let result = async {
                loop {
                    if cancel.is_cancelled() { return Err(ProviderError::Cancelled); }
                    if Instant::now() >= deadline { return Err(ProviderError::Transport("request timeout".into())); }
                    if let Some(event) = pending.pop_front() { return map_event(event, &key); }
                    let chunk = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep_until(deadline) => return Err(ProviderError::Transport("request timeout".into())),
                        _ = tokio::time::sleep_until(idle_deadline) => return Err(ProviderError::IdleTimeout),
                        chunk = response.as_mut().expect("response is held until termination").chunk() => chunk.map_err(error::transport)?,
                    };
                    let Some(chunk) = chunk else {
                        return Err(ProviderError::Protocol("SSE ended before message_stop".into()));
                    };
                    let events = parser.feed(&chunk)?;
                    if !events.is_empty() { idle_deadline = Instant::now() + idle_timeout; }
                    pending.extend(events);
                }
            }.await;
            match result {
                Ok(events) => for event in events {
                    let stopped = matches!(event, StreamEvent::MessageStop);
                    if stopped { drop(response.take()); }
                    yield Ok(event);
                    if stopped { return; }
                },
                Err(error) => {
                    // Close the connection before yielding a terminal error, even
                    // if the caller keeps the stream without polling it again.
                    drop(response.take());
                    yield Err(error);
                    return;
                }
            }
        }
    })
}

fn map_event(event: Event, key: &str) -> Result<Vec<StreamEvent>, ProviderError> {
    if event.event == "ping" {
        return Ok(vec![]);
    }
    let value: Value = serde_json::from_str(&event.data)
        .map_err(|_| ProviderError::Protocol("invalid SSE JSON".into()))?;
    let kind = if event.event == "message" {
        string(&value, "type")?
    } else {
        &event.event
    };
    let index = || {
        value["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| malformed("index"))
    };
    let mapped = match kind {
        "message_start" => {
            let message = &value["message"];
            StreamEvent::MessageStart {
                id: message["id"].as_str().map(str::to_owned),
                model: string(message, "model")?.into(),
                usage: usage(&message["usage"])?,
            }
        }
        "content_block_start" => {
            let raw = &value["content_block"];
            let kind = string(raw, "type")?;
            let block = match kind {
                "text" => BlockStart::Text,
                "thinking" => BlockStart::Thinking,
                "tool_use" => BlockStart::ToolUse {
                    id: string(raw, "id")?.into(),
                    name: string(raw, "name")?.into(),
                },
                _ => BlockStart::Opaque {
                    provider: "anthropic".into(),
                    kind: kind.into(),
                    raw: raw.clone(),
                },
            };
            StreamEvent::BlockStart {
                index: index()?,
                block,
            }
        }
        "content_block_delta" => {
            let delta = &value["delta"];
            match string(delta, "type")? {
                "text_delta" => StreamEvent::TextDelta {
                    index: index()?,
                    text: string(delta, "text")?.into(),
                },
                "thinking_delta" => StreamEvent::ThinkingDelta {
                    index: index()?,
                    thinking: string(delta, "thinking")?.into(),
                },
                "signature_delta" => StreamEvent::SignatureDelta {
                    index: index()?,
                    signature: string(delta, "signature")?.into(),
                },
                "input_json_delta" => StreamEvent::ToolInputDelta {
                    index: index()?,
                    partial_json: string(delta, "partial_json")?.into(),
                },
                _ => {
                    tracing::debug!("ignoring unknown Anthropic delta type");
                    return Ok(vec![]);
                }
            }
        }
        "content_block_stop" => StreamEvent::BlockStop { index: index()? },
        "message_delta" => {
            let reason = &value["delta"]["stop_reason"];
            let stop_reason = if reason.is_null() {
                None
            } else {
                Some(serde_json::from_value(reason.clone()).map_err(|_| malformed("stop_reason"))?)
            };
            StreamEvent::MessageDelta {
                stop_reason,
                usage: usage(&value["usage"])?,
            }
        }
        "message_stop" => StreamEvent::MessageStop,
        "ping" => return Ok(vec![]),
        "error" => {
            return Err(ProviderError::Stream {
                kind: error::redact(string(&value["error"], "type")?, key),
                message: error::redact(string(&value["error"], "message")?, key),
            });
        }
        _ => {
            tracing::debug!("ignoring unknown Anthropic event type");
            return Ok(vec![]);
        }
    };
    Ok(vec![mapped])
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value[field].as_str().ok_or_else(|| malformed(field))
}

fn malformed(field: &str) -> ProviderError {
    ProviderError::Protocol(format!("invalid Anthropic event field: {field}"))
}

fn usage(value: &Value) -> Result<Usage, ProviderError> {
    if let Some(iterations) = value.get("iterations") {
        let iterations = iterations
            .as_array()
            .ok_or_else(|| malformed("iterations"))?;
        iterations
            .iter()
            .try_fold(Usage::default(), |mut sum, iteration| {
                let next = counters(iteration)?;
                sum.input_tokens = sum
                    .input_tokens
                    .checked_add(next.input_tokens)
                    .ok_or_else(|| malformed("usage overflow"))?;
                sum.output_tokens = sum
                    .output_tokens
                    .checked_add(next.output_tokens)
                    .ok_or_else(|| malformed("usage overflow"))?;
                sum.cache_creation_input_tokens = sum
                    .cache_creation_input_tokens
                    .checked_add(next.cache_creation_input_tokens)
                    .ok_or_else(|| malformed("usage overflow"))?;
                sum.cache_read_input_tokens = sum
                    .cache_read_input_tokens
                    .checked_add(next.cache_read_input_tokens)
                    .ok_or_else(|| malformed("usage overflow"))?;
                Ok(sum)
            })
    } else {
        counters(value)
    }
}

fn counters(value: &Value) -> Result<Usage, ProviderError> {
    if value.is_null() {
        return Ok(Usage::default());
    }
    serde_json::from_value(value.clone()).map_err(|_| malformed("usage"))
}
