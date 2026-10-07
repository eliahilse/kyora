//! Child-agent requests, shared handles and typed recursion errors.
use crate::{AgentOutcome, ModelRef, NodeId, Status, ToolSelection};
use kyora_protocol::Usage;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Ownership of a child agent's lifetime.
#[derive(Debug, Clone)]
pub enum Owner {
    /// Cancel when the calling cell ends, or when the parent node ends.
    /// The caller consumes the result through the handle; no notice is posted.
    Cell(CancellationToken),
    /// Keep running until explicitly cancelled or the parent node ends.
    /// The child's result, error or cancellation arrives in the parent's mailbox.
    Node,
}
/// Optional child settings. Unspecified model and deadline inherit from the parent.
#[derive(Debug, Clone, Default)]
pub struct ChildSpec {
    /// First user task.
    pub task: String,
    /// Optional trace display name.
    pub name: Option<String>,
    /// Model override, otherwise the parent's model.
    pub model: Option<ModelRef>,
    /// Explicit subset of parent tools; None uses the default sub-agent rule.
    pub tools: ToolSelection,
    /// Turn cap override, otherwise Limits::subagent_max_turns.
    pub max_turns: Option<u32>,
    /// Subtree token budget, bounded by every ancestor.
    pub budget: Option<u64>,
    /// Child timeout, capped by the parent's deadline.
    pub timeout: Option<Duration>,
    /// Opaque data forwarded to the node's toolset factory.
    pub init: Option<Arc<serde_json::Value>>,
    /// Text appended after the task in the first user message.
    pub preamble: Option<String>,
    /// Originating cell, recorded in the trace.
    pub origin_cell: Option<u32>,
    /// JSON schema (type object) for a structured result. The child gets a
    /// `submit_result` tool with this input schema; a valid submission ends it.
    pub output: Option<serde_json::Value>,
}
impl ChildSpec {
    /// Creates a task using inherited settings and the default child tool rule.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            ..Self::default()
        }
    }
}
/// Recoverable admission and leaf-call errors for recursive adapters.
#[derive(Debug, thiserror::Error)]
pub enum RecursionError {
    /// An atomic admission limit refused the request without starting work.
    #[error("limit exceeded: {limit}")]
    LimitExceeded {
        /// One of depth, agents_live, agents_total or llm_calls.
        limit: &'static str,
    },
    /// No dispatch headroom remains in a scope or ancestor.
    #[error("budget exhausted")]
    BudgetExceeded,
    /// Arguments or capabilities are invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The node or request owner has ended.
    #[error("cancelled")]
    Cancelled,
    /// A leaf provider failed or refused the request.
    #[error("model error: {0}")]
    ModelError(String),
    /// The recipient's mailbox already holds its capacity of undelivered messages.
    #[error("mailbox of agent {agent} is full")]
    MailboxFull {
        /// Recipient.
        agent: NodeId,
    },
    /// The recipient has finished and accepts no further messages.
    #[error("agent {agent} has finished")]
    AgentFinished {
        /// Recipient.
        agent: NodeId,
    },
}
/// How an agent stopped after `NodeCtx::cancel_agent`.
#[derive(Debug, Clone)]
pub struct CancelOutcome {
    /// Final outcome. An agent that finished before the cancellation reached it
    /// keeps its own status.
    pub outcome: AgentOutcome,
    /// The agent had already finished when it was cancelled.
    pub already_finished: bool,
    /// For a direct child, what it had queued for the caller: its unread messages
    /// and its terminal notice, in arrival order.
    pub messages: Vec<crate::Envelope>,
}
/// Live counters for a child; status is absent until shutdown completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildStatus {
    /// Terminal status, or None while running or shutting down.
    pub status: Option<Status>,
    /// Admitted assistant turns so far.
    pub turns: u32,
    /// Direct measured or conservatively charged usage.
    pub usage_self: Usage,
    /// Charges for this node and every descendant.
    pub usage_subtree: Usage,
}
/// Cheaply cloned handle with a retained result shared by every waiter.
#[derive(Clone)]
pub struct AgentHandle {
    /// Session-local child identifier.
    pub id: NodeId,
    pub(crate) node: Arc<crate::NodeCtx>,
    pub(crate) outcome: watch::Receiver<Option<AgentOutcome>>,
}
impl AgentHandle {
    /// Waits for shutdown and returns the same outcome to every caller.
    pub async fn result(&self) -> AgentOutcome {
        let mut receiver = self.outcome.clone();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            receiver
                .changed()
                .await
                .expect("agent task retains its result sender");
        }
    }
    /// Returns current ledger usage and admitted turns.
    pub fn status(&self) -> ChildStatus {
        self.node.child_status(self.outcome.borrow().as_ref())
    }
    /// Cancels the child and all of its descendants.
    pub fn cancel(&self) {
        self.node.cancel.cancel();
    }
    /// True once descendant cleanup, settlement and node_end have finished.
    pub fn is_finished(&self) -> bool {
        self.outcome.borrow().is_some()
    }
}
