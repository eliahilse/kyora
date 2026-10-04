//! Integration boundary for a future core-loop adapter.

/// Session-wide UI identifier. Cells and model calls also receive distinct IDs.
pub type NodeId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    Done,
    Cancelled,
    Failed,
}

impl Status {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Agent,
    Cell,
    Llm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSpec {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub name: String,
    pub model: String,
}

/// Ordered events for one turn. Usage values are cumulative per node, so
/// replaying a usage snapshot does not double-charge session totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    TextDelta {
        node: NodeId,
        text: String,
    },
    ToolCallStarted {
        node: NodeId,
        id: String,
        name: String,
        args: String,
    },
    ToolCallFinished {
        node: NodeId,
        id: String,
        result: String,
        status: Status,
    },
    AgentSpawned(NodeSpec),
    AgentFinished {
        node: NodeId,
        status: Status,
    },
    ReplCellStarted {
        node: NodeSpec,
        code: String,
    },
    ReplCellFinished {
        node: NodeId,
        output: String,
        status: Status,
    },
    LlmCall {
        node: NodeSpec,
    },
    LlmCallFinished {
        node: NodeId,
        status: Status,
    },
    Usage {
        node: NodeId,
        tokens: u64,
        cost_microusd: u64,
    },
    /// None updates the shared budget; Some sets a tighter node budget.
    Budget {
        node: Option<NodeId>,
        remaining: u64,
    },
}
