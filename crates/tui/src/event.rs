//! Integration boundary for a future core-loop adapter.

/// Session-wide UI identifier. Cells and model calls also receive distinct IDs.
pub type NodeId = u64;

/// Lifecycle state displayed for a node or tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting for a turn.
    Idle,
    /// Work is in progress.
    Running,
    /// Work completed successfully.
    Done,
    /// Work was cancelled.
    Cancelled,
    /// Work failed.
    Failed,
}

impl Status {
    /// Returns the short status text used in the terminal.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// Kind of work represented by a recursion tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// An agent with its own conversation.
    Agent,
    /// A Python REPL cell.
    Cell,
    /// A leaf model completion.
    Llm,
}

/// Node identity, parent and display metadata supplied by an event source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSpec {
    /// Unique UI node identifier.
    pub id: NodeId,
    /// Parent identifier, or none for a root.
    pub parent: Option<NodeId>,
    /// Display name.
    pub name: String,
    /// Displayed model reference.
    pub model: String,
}

/// Ordered events for one turn. Usage values are cumulative per node, so
/// replaying a usage snapshot does not double-charge session totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Appends streamed assistant text to a node's transcript.
    TextDelta {
        /// Node producing the text.
        node: NodeId,
        /// Newly received text.
        text: String,
    },
    /// Adds a running tool call to a node's transcript.
    ToolCallStarted {
        /// Node invoking the tool.
        node: NodeId,
        /// Tool call identifier within the node.
        id: String,
        /// Tool name.
        name: String,
        /// Displayed tool arguments.
        args: String,
    },
    /// Completes an existing tool call.
    ToolCallFinished {
        /// Node that invoked the tool.
        node: NodeId,
        /// Identifier of the started call.
        id: String,
        /// Displayed tool result.
        result: String,
        /// Call's final state.
        status: Status,
    },
    /// Admits an agent to the recursion tree.
    AgentSpawned(NodeSpec),
    /// Updates an agent's final state.
    AgentFinished {
        /// Agent identifier.
        node: NodeId,
        /// Agent's final state.
        status: Status,
    },
    /// Admits a REPL cell and records its source in the transcript.
    ReplCellStarted {
        /// Cell identity and display metadata.
        node: NodeSpec,
        /// Executed Python source.
        code: String,
    },
    /// Completes a REPL cell and appends its output.
    ReplCellFinished {
        /// Cell identifier.
        node: NodeId,
        /// Captured cell output.
        output: String,
        /// Cell's final state.
        status: Status,
    },
    /// Admits a leaf model call to the recursion tree.
    LlmCall {
        /// Call identity and display metadata.
        node: NodeSpec,
    },
    /// Updates a leaf model call's final state.
    LlmCallFinished {
        /// Call identifier.
        node: NodeId,
        /// Call's final state.
        status: Status,
    },
    /// Replaces a node's cumulative usage snapshot.
    Usage {
        /// Node whose usage is updated.
        node: NodeId,
        /// Cumulative token usage for the node.
        tokens: u64,
        /// Cumulative estimated cost in millionths of a US dollar.
        cost_microusd: u64,
    },
    /// None updates the shared budget; Some sets a tighter node budget.
    Budget {
        /// Node with a tighter budget, or none for the shared budget.
        node: Option<NodeId>,
        /// Remaining tokens.
        remaining: u64,
    },
}
