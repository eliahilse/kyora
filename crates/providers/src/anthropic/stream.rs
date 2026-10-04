use std::{collections::HashMap, time::Duration};

use kyora_protocol::{BlockStart, StreamEvent, Usage, UsageIteration};
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
        let mut mapper = Mapper::default();
        let mut pending = std::collections::VecDeque::new();
        let mut idle_deadline = Instant::now() + idle_timeout;
        loop {
            let result = async {
                loop {
                    if cancel.is_cancelled() { return Err(ProviderError::Cancelled); }
                    if Instant::now() >= deadline { return Err(ProviderError::Transport("request timeout".into())); }
                    if let Some(event) = pending.pop_front() { return mapper.map_event(event, &key); }
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

#[derive(Default)]
struct Mapper {
    server_tools: HashMap<usize, (Value, String)>,
    model: String,
    usage: Usage,
}

impl Mapper {
    fn map_event(&mut self, event: Event, key: &str) -> Result<Vec<StreamEvent>, ProviderError> {
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
                self.model = string(message, "model")?.into();
                self.usage = counters(&message["usage"])?;
                StreamEvent::MessageStart {
                    id: message["id"].as_str().map(str::to_owned),
                    model: string(message, "model")?.into(),
                    usage: counters(&message["usage"])?,
                }
            }
            "content_block_start" => {
                let raw = &value["content_block"];
                let kind = string(raw, "type")?;
                if kind == "server_tool_use" {
                    if self
                        .server_tools
                        .insert(index()?, (raw.clone(), String::new()))
                        .is_some()
                    {
                        return Err(malformed("duplicate server tool index"));
                    }
                    return Ok(vec![]);
                }
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
                if let Some((_, partial)) = self.server_tools.get_mut(&index()?) {
                    if string(delta, "type")? == "input_json_delta" {
                        partial.push_str(string(delta, "partial_json")?);
                    }
                    return Ok(vec![]);
                }
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
            "content_block_stop" => {
                let index = index()?;
                if let Some((mut raw, partial)) = self.server_tools.remove(&index) {
                    if !partial.is_empty() {
                        raw["input"] = serde_json::from_str(&partial)
                            .map_err(|_| malformed("server tool input"))?;
                    }
                    return Ok(vec![
                        StreamEvent::BlockStart {
                            index,
                            block: BlockStart::Opaque {
                                provider: "anthropic".into(),
                                kind: "server_tool_use".into(),
                                raw,
                            },
                        },
                        StreamEvent::BlockStop { index },
                    ]);
                }
                StreamEvent::BlockStop { index }
            }
            "message_delta" => {
                let reason = &value["delta"]["stop_reason"];
                let stop_reason = if reason.is_null() {
                    None
                } else {
                    Some(
                        serde_json::from_value(reason.clone())
                            .map_err(|_| malformed("stop_reason"))?,
                    )
                };
                StreamEvent::MessageDelta {
                    stop_reason,
                    usage: counters(&value["usage"])?,
                }
            }
            "message_stop" => {
                if !self.server_tools.is_empty() {
                    return Err(malformed("open server tool block"));
                }
                StreamEvent::MessageStop
            }
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
        let mut events = vec![mapped];
        let usage = match kind {
            "message_start" => &value["message"]["usage"],
            "message_delta" => &value["usage"],
            _ => return Ok(events),
        };
        let mut latest = counters(usage)?;
        if let Some(iterations) = usage.get("iterations").filter(|value| !value.is_null()) {
            let iterations: Vec<UsageIteration> =
                serde_json::from_value(iterations.clone()).map_err(|_| malformed("iterations"))?;
            let serving = iterations
                .iter()
                .rev()
                .find(|iteration| iteration.kind == "fallback_message");
            if let Some(serving) = serving {
                if serving.model.is_empty() {
                    return Err(malformed("fallback model"));
                }
                self.model.clone_from(&serving.model);
            }
            // Streaming updates can omit counters. Fill those from the serving
            // iteration, never from the earlier model after a handoff.
            fill_missing_counters(
                &mut latest,
                usage,
                serving.map_or(self.usage, |entry| entry.usage),
            );
            // Top-level counts belong only to the returned message. Iterations
            // retain attribution and can include refusals that are not billed.
            events.push(StreamEvent::UsageSnapshot {
                model: self.model.clone(),
                usage: latest,
                iterations,
            });
        } else {
            fill_missing_counters(&mut latest, usage, self.usage);
        }
        self.usage = latest;
        Ok(events)
    }
}

fn fill_missing_counters(latest: &mut Usage, raw: &Value, previous: Usage) {
    for (field, current, fallback) in [
        (
            "input_tokens",
            &mut latest.input_tokens,
            previous.input_tokens,
        ),
        (
            "output_tokens",
            &mut latest.output_tokens,
            previous.output_tokens,
        ),
        (
            "cache_creation_input_tokens",
            &mut latest.cache_creation_input_tokens,
            previous.cache_creation_input_tokens,
        ),
        (
            "cache_read_input_tokens",
            &mut latest.cache_read_input_tokens,
            previous.cache_read_input_tokens,
        ),
    ] {
        if raw.get(field).is_none() {
            *current = fallback;
        }
    }
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value[field].as_str().ok_or_else(|| malformed(field))
}

fn malformed(field: &str) -> ProviderError {
    ProviderError::Protocol(format!("invalid Anthropic event field: {field}"))
}

fn counters(value: &Value) -> Result<Usage, ProviderError> {
    if value.is_null() {
        return Ok(Usage::default());
    }
    serde_json::from_value(value.clone()).map_err(|_| malformed("usage"))
}
