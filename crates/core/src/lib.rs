//! Runtime, tools, accounting and durable traces for local agent invocations.
pub mod agent_tools;
pub mod defaults;
pub mod ledger;
pub mod messages;
pub mod prompts;
pub mod recursion;
pub mod runtime;
pub mod session;
pub mod tool;
pub mod trace;
pub use defaults::{Limits, ModelRef};
pub use ledger::{BudgetSnapshot, Ledger, NodeId};
pub use messages::{Delivery, Envelope, MessageId, MessageKind, Waited};
pub use runtime::{
    AgentOutcome, AgentSpec, Answer, LlmCall, LlmOutcome, NodeCtx, NodeInfo, Runtime,
    RuntimeConfig, Status,
};
pub use tool::{Effect, Tool, ToolCx, ToolOutput, ToolSelection, Toolset, ToolsetFactory};
pub use trace::{
    RecursionTree, TraceEvent, TraceRecord, TraceSink, reconstruct_jsonl, reconstruct_tree,
};

pub use recursion::{AgentHandle, ChildSpec, ChildStatus, Owner, RecursionError};
