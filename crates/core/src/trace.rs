//! Shared persisted and live trace events. Stream events are ephemeral.
use crate::{AgentOutcome, Limits, NodeId, Status, defaults};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use kyora_protocol::{Message, StopReason, StreamEvent, ToolSpec, Usage};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, mpsc, oneshot};

/// One lifecycle, accounting, tool or streaming event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEvent {
    /// Begins a session.
    SessionStart {
        /// UUIDv7 session identifier.
        session: String,
        /// Workspace path.
        cwd: PathBuf,
        /// Runtime version.
        kyora: String,
        /// Validated limits.
        limits: Limits,
    },
    /// Freezes a node's request prefix.
    NodeStart {
        /// Node identifier.
        node: NodeId,
        /// Parent identifier.
        parent: Option<NodeId>,
        /// Agent depth.
        depth: u32,
        /// Agent or llm.
        kind: String,
        /// Display name.
        name: String,
        /// Provider/model reference.
        model: String,
        /// Frozen system prompt.
        system: Option<String>,
        /// Frozen tool specifications.
        tools: Vec<ToolSpec>,
        /// Node limits.
        limits: Limits,
        /// Leaf prompt; stored as a blob if large.
        prompt: Option<serde_json::Value>,
    },
    /// Appends a history message, kept inline for replay.
    Message {
        /// Owning node.
        node: NodeId,
        /// Verbatim admitted message.
        message: Message,
    },
    /// Write-ahead reservation, flushed before provider dispatch.
    AttemptStart {
        /// Owning node.
        node: NodeId,
        /// Unique attempt id.
        attempt: u64,
        /// Provider/model reference.
        model: String,
        /// Reserved processed tokens.
        reserved: u64,
        /// Actual request output cap.
        max_tokens: u32,
    },
    /// Settlement for every started attempt.
    AttemptEnd {
        /// Owning node.
        node: NodeId,
        /// Attempt id paired with attempt_start.
        attempt: u64,
        /// Provider response id when known.
        request_id: Option<String>,
        /// Completed or a provider error.
        outcome: String,
        /// Recorded charge.
        charged: u64,
        /// Recorded charge above the reservation.
        excess: u64,
        /// Actual usage when known.
        usage: Option<Usage>,
        /// Stop reason when known.
        stop_reason: Option<StopReason>,
        /// Elapsed milliseconds.
        ms: u64,
    },
    /// Preserves raw malformed input, including responses rejected before admission.
    InvalidToolInput {
        /// Owning node.
        node: NodeId,
        /// Tool-use identifier.
        call: String,
        /// Original tool input text.
        raw: String,
    },
    /// Reports a terminal request or runtime error.
    Error {
        /// Owning node.
        node: NodeId,
        /// Error description.
        message: String,
    },
    /// Records one admitted tool call before execution.
    ToolCall {
        /// Owning node.
        node: NodeId,
        /// Tool-use id.
        call: String,
        /// Tool name.
        name: String,
        /// Arguments, including raw malformed input if present.
        input: serde_json::Value,
    },
    /// Records one tool result.
    ToolResult {
        /// Owning node.
        node: NodeId,
        /// Tool-use id.
        call: String,
        /// Bounded output text.
        content: String,
        /// Execution failed.
        is_error: bool,
    },
    /// Node shutdown and final accounting.
    NodeEnd {
        /// Final node outcome.
        #[serde(flatten)]
        outcome: AgentOutcome,
    },
    /// Ends a session after root shutdown.
    SessionEnd {
        /// Root status.
        status: Status,
    },
    /// Provider stream fragment, never persisted.
    Delta {
        /// Owning node.
        node: NodeId,
        /// Provider-neutral fragment.
        event: StreamEvent,
    },
    /// Frontends discard partial output before a retry; never persisted.
    StreamReset {
        /// Owning node.
        node: NodeId,
    },
}
impl TraceEvent {
    /// Whether this event is broadcast without writing a transcript record.
    pub fn ephemeral(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::StreamReset { .. })
    }
}
/// Common JSONL envelope. Ephemeral live records have no sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Format version.
    pub v: u32,
    /// Gapless persisted sequence; absent on ephemeral events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    /// Event fields.
    #[serde(flatten)]
    pub event: TraceEvent,
}
impl TraceRecord {
    pub(crate) fn new(event: TraceEvent, seq: Option<u64>) -> Self {
        Self {
            v: 1,
            seq,
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            event,
        }
    }
}
pub(crate) enum WriteCommand {
    Event(Box<TraceEvent>, oneshot::Sender<Result<()>>),
    Finish(oneshot::Sender<Result<()>>),
}
struct Inner {
    live: broadcast::Sender<TraceRecord>,
    writer: Option<mpsc::Sender<WriteCommand>>,
    join: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
/// A bounded broadcast channel and optional single durable writer.
#[derive(Clone)]
pub struct TraceSink(Arc<Inner>);
impl TraceSink {
    /// Creates a broadcast-only trace sink, writing no files.
    pub fn ephemeral() -> Self {
        let (live, _) = broadcast::channel(defaults::TRACE_CAPACITY);
        Self(Arc::new(Inner {
            live,
            writer: None,
            join: tokio::sync::Mutex::new(None),
        }))
    }
    pub(crate) fn writer(
        live: broadcast::Sender<TraceRecord>,
        writer: mpsc::Sender<WriteCommand>,
        join: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self(Arc::new(Inner {
            live,
            writer: Some(writer),
            join: tokio::sync::Mutex::new(Some(join)),
        }))
    }
    /// Subscribes to the shared live event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<TraceRecord> {
        self.0.live.subscribe()
    }
    /// Emits an event. Persistent events return only after the record is flushed.
    pub async fn emit(&self, event: TraceEvent) -> Result<()> {
        if event.ephemeral() || self.0.writer.is_none() {
            let _ = self.0.live.send(TraceRecord::new(event, None));
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.0
            .writer
            .as_ref()
            .expect("writer present")
            .send(WriteCommand::Event(Box::new(event), tx))
            .await?;
        rx.await?
    }
    /// Flushes and joins the writer, releasing its exclusive file lock.
    pub async fn finish(&self) -> Result<()> {
        let mut join = self.0.join.lock().await;
        if let Some(handle) = join.take() {
            let (tx, rx) = oneshot::channel();
            self.0
                .writer
                .as_ref()
                .expect("writer present")
                .send(WriteCommand::Finish(tx))
                .await?;
            let result = rx.await?;
            handle.await?;
            result?;
        }
        Ok(())
    }
}
