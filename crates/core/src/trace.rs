//! Shared persisted and live trace events. Stream events are ephemeral.
use crate::{
    AgentOutcome, Limits, NodeId, Status, defaults,
    messages::{Delivery, Envelope, MessageId},
};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use futures::FutureExt;
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
        /// Originating REPL cell, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_cell: Option<u32>,
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
    /// A message accepted into its recipient's mailbox, recorded before it can be delivered.
    MessageSent {
        /// The accepted envelope.
        #[serde(flatten)]
        message: Envelope,
    },
    /// Messages taken from an agent's mailbox, in delivery order.
    MessageDelivered {
        /// Recipient.
        node: NodeId,
        /// Delivered message ids.
        messages: Vec<MessageId>,
        /// Turn boundary, receive or wait.
        via: Delivery,
    },
    /// A message that never reached its recipient, such as one still queued when the
    /// recipient ended or a notice for a parent that had already ended.
    MessageUndelivered {
        /// The envelope, including its body.
        #[serde(flatten)]
        message: Envelope,
        /// Why it was not delivered.
        reason: String,
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
    /// Creates a durable sink over caller-provided storage. `store` receives each
    /// persistent record in sequence order and returns its acknowledgement; `emit`
    /// for that event returns, with the acknowledgement's result, once it completes.
    /// Acknowledgements may complete in any order, so storage can confirm records
    /// asynchronously. Records are broadcast to subscribers as they are handed over.
    /// The first failed acknowledgement, including a store future that panics, is
    /// latched: later events fail without being stored, and `finish` returns it. An acknowledgement that never completes
    /// blocks the node that emitted it, and so shutdown.
    pub fn with_store<F, Fut>(mut store: F) -> Self
    where
        F: FnMut(TraceRecord) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let (live, _) = broadcast::channel(defaults::TRACE_CAPACITY);
        let (writer, mut rx) = mpsc::channel(defaults::WRITER_CAPACITY);
        let broadcast = live.clone();
        let join = tokio::spawn(async move {
            let mut seq = 0;
            let mut acks = tokio::task::JoinSet::new();
            let failed = Arc::new(std::sync::Mutex::new(None::<String>));
            let latched = |failed: &std::sync::Mutex<Option<String>>| {
                failed
                    .lock()
                    .expect("trace failure mutex poisoned")
                    .clone()
                    .map(|error| anyhow::anyhow!(error))
            };
            // An acknowledgement task that did not complete dropped its ack; that is a
            // storage failure too.
            let lost = |failed: &std::sync::Mutex<Option<String>>| {
                failed
                    .lock()
                    .expect("trace failure mutex poisoned")
                    .get_or_insert_with(|| "trace acknowledgement was lost".into());
            };
            while let Some(command) = rx.recv().await {
                while let Some(joined) = acks.try_join_next() {
                    if joined.is_err() {
                        lost(&failed);
                    }
                }
                match command {
                    WriteCommand::Event(event, ack) => {
                        // After a failure nothing else is stored, as with a session file.
                        if let Some(error) = latched(&failed) {
                            let _ = ack.send(Err(error));
                            continue;
                        }
                        let record = TraceRecord::new(*event, Some(seq));
                        seq += 1;
                        let _ = broadcast.send(record.clone());
                        let stored = store(record);
                        let failed = failed.clone();
                        acks.spawn(async move {
                            // A store that panics has failed like one that returns an error.
                            let result = std::panic::AssertUnwindSafe(stored)
                                .catch_unwind()
                                .await
                                .unwrap_or_else(|_| Err(anyhow::anyhow!("trace store panicked")));
                            if let Err(error) = &result {
                                failed
                                    .lock()
                                    .expect("trace failure mutex poisoned")
                                    .get_or_insert_with(|| error.to_string());
                            }
                            let _ = ack.send(result);
                        });
                    }
                    WriteCommand::Finish(ack) => {
                        while let Some(joined) = acks.join_next().await {
                            if joined.is_err() {
                                lost(&failed);
                            }
                        }
                        let _ = ack.send(latched(&failed).map_or(Ok(()), Err));
                        break;
                    }
                }
            }
        });
        Self::writer(live, writer, join)
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

/// Serializable recursion node reconstructed from lifecycle and accounting records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecursionTree {
    /// Session-local identifier.
    pub id: NodeId,
    /// Agent or llm.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Provider/model reference.
    pub model: String,
    /// Originating cell, when recorded.
    pub origin_cell: Option<u32>,
    /// Terminal status, or None for a running node.
    pub status: Option<Status>,
    /// Admitted assistant turns.
    pub turns: u32,
    /// Usage charged directly to this node.
    pub usage_self: Usage,
    /// Direct usage plus all descendant charges.
    pub usage_subtree: Usage,
    /// Children sorted by session-local id.
    pub children: Vec<RecursionTree>,
}

/// Reconstructs a forest from session records without IO or dependence on event scheduling.
/// Rejects duplicate starts, missing parents and cycles. Partial traces retain running status.
pub fn reconstruct_tree(records: &[TraceRecord]) -> Result<Vec<RecursionTree>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut nodes = BTreeMap::new();
    for record in records {
        if let TraceEvent::NodeStart {
            node,
            parent,
            kind,
            name,
            model,
            origin_cell,
            ..
        } = &record.event
        {
            let tree = RecursionTree {
                id: *node,
                kind: kind.clone(),
                name: name.clone(),
                model: model.clone(),
                origin_cell: *origin_cell,
                status: None,
                turns: 0,
                usage_self: Usage::default(),
                usage_subtree: Usage::default(),
                children: Vec::new(),
            };
            if nodes.insert(*node, (*parent, tree)).is_some() {
                anyhow::bail!("duplicate node_start: {node}");
            }
        }
    }
    let mut settled = BTreeSet::new();
    for record in records {
        match &record.event {
            TraceEvent::Message { node, message }
                if message.role == kyora_protocol::Role::Assistant =>
            {
                if let Some((_, tree)) = nodes.get_mut(node) {
                    tree.turns += 1;
                }
            }
            TraceEvent::AttemptEnd {
                node,
                attempt,
                usage,
                charged,
                ..
            } if settled.insert(*attempt) => {
                if let Some((_, tree)) = nodes.get_mut(node) {
                    tree.usage_self += usage.unwrap_or(Usage {
                        input_tokens: *charged,
                        ..Usage::default()
                    });
                }
            }
            _ => {}
        }
    }
    for record in records {
        if let TraceEvent::NodeEnd { outcome } = &record.event
            && let Some((_, tree)) = nodes.get_mut(&outcome.node)
        {
            tree.status = Some(outcome.status);
            tree.turns = outcome.turns;
            tree.usage_self = outcome.usage_self;
        }
    }
    let mut children: BTreeMap<Option<NodeId>, Vec<NodeId>> = BTreeMap::new();
    for (id, (parent, _)) in &nodes {
        if parent.is_some_and(|parent| !nodes.contains_key(&parent)) {
            anyhow::bail!("missing parent for node: {id}");
        }
        children.entry(*parent).or_default().push(*id);
    }
    fn build(
        id: NodeId,
        nodes: &mut BTreeMap<NodeId, (Option<NodeId>, RecursionTree)>,
        children: &BTreeMap<Option<NodeId>, Vec<NodeId>>,
    ) -> RecursionTree {
        let (_, mut tree) = nodes.remove(&id).expect("acyclic node tree");
        tree.usage_subtree = tree.usage_self;
        for child in children.get(&Some(id)).into_iter().flatten() {
            let child = build(*child, nodes, children);
            tree.usage_subtree += child.usage_subtree;
            tree.children.push(child);
        }
        tree
    }
    let roots = children.get(&None).cloned().unwrap_or_default();
    // Verify ancestry before recursive construction, including disconnected cycles.
    for id in nodes.keys() {
        let mut seen = BTreeSet::new();
        let mut next = Some(*id);
        while let Some(id) = next {
            if !seen.insert(id) {
                anyhow::bail!("cycle at node: {id}");
            }
            next = nodes[&id].0;
        }
    }
    Ok(roots
        .into_iter()
        .map(|id| build(id, &mut nodes, &children))
        .collect())
}

/// Parses events.jsonl contents and reconstructs the tree without reading any files.
pub fn reconstruct_jsonl(jsonl: &str) -> Result<Vec<RecursionTree>> {
    let records = jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<TraceRecord>, _>>()?;
    reconstruct_tree(&records)
}
