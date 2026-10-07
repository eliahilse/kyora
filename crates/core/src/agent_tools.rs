//! Model-facing tools for sub-agents and messages within one agent tree.
//!
//! Thin adapters over `NodeCtx::spawn_agent`, `send`, `receive` and `wait`. They
//! hold no state of their own, so one set can serve every node of a toolset factory.
use crate::{ChildSpec, Effect, Owner, Tool, ToolCx, ToolOutput, ToolSelection};
use async_trait::async_trait;
use kyora_protocol::ToolSpec;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// Names of the agent tools, in the order `tools` returns them.
pub const NAMES: &[&str] = &["spawn_agent", "send_message", "receive", "wait"];

/// Returns the four agent tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SpawnAgent),
        Arc::new(SendMessage),
        Arc::new(Receive),
        Arc::new(Wait),
    ]
}
fn spec(name: &str, description: &str, properties: Value, required: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        large_input: false,
    }
}
/// Reads an optional non-negative number of seconds.
fn seconds(input: &Value, key: &str) -> Result<Option<Duration>, String> {
    match input.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .map(Some)
            .ok_or_else(|| format!("{key} must be a non-negative number of seconds")),
    }
}

/// Starts a node-owned child and returns its id at once.
pub struct SpawnAgent;
#[async_trait]
impl Tool for SpawnAgent {
    fn spec(&self) -> ToolSpec {
        spec(
            "spawn_agent",
            "Start a sub-agent on a task and return its id at once. It runs in the background while you keep working; its result arrives later as a message at the start of one of your turns. If you end your turn while sub-agents are running, you wait for their results.",
            json!({"task":{"type":"string","description":"Complete instructions. The sub-agent sees nothing else of your conversation."},"name":{"type":"string","description":"Short name to address it by."},"tools":{"type":"array","items":{"type":"string"},"description":"Tools to give it, a subset of yours. Omit for the default set."},"budget":{"type":"integer","description":"Token budget for it and its own sub-agents."},"timeout":{"type":"number","description":"Seconds before it is stopped."}}),
            json!(["task"]),
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let mut child = ChildSpec::new(input["task"].as_str().unwrap_or_default());
        child.name = input["name"].as_str().map(Into::into);
        if let Some(tools) = input["tools"].as_array() {
            child.tools = ToolSelection(Some(
                tools
                    .iter()
                    .filter_map(|tool| tool.as_str().map(Into::into))
                    .collect(),
            ));
        }
        if let Some(budget) = input.get("budget") {
            match budget.as_u64() {
                Some(budget) => child.budget = Some(budget),
                None => return ToolOutput::error("budget must be a positive integer"),
            }
        }
        match seconds(&input, "timeout") {
            Ok(timeout) => child.timeout = timeout,
            Err(error) => return ToolOutput::error(error),
        }
        let name = child.name.clone().unwrap_or_default();
        match cx.node.spawn_agent(child, Owner::Node) {
            Ok(handle) if name.is_empty() => {
                ToolOutput::text(format!("started agent {}", handle.id))
            }
            Ok(handle) => ToolOutput::text(format!("started agent {} ({name})", handle.id)),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// Queues a message for the parent, a child or a sibling.
pub struct SendMessage;
#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        spec(
            "send_message",
            "Send a message to your parent, one of your sub-agents or a sibling. The recipient sees it at the start of its next turn; this call does not wait for a reply.",
            json!({"to":{"type":"string","description":"\"parent\", an agent id, or an agent name."},"body":{"type":"string"}}),
            json!(["to", "body"]),
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let body = input["body"].as_str().unwrap_or_default();
        let sent = match cx.node.resolve(input["to"].as_str().unwrap_or_default()) {
            Ok(to) => cx.node.send(to, body).await.map(|id| (to, id)),
            Err(error) => Err(error),
        };
        match sent {
            Ok((to, id)) => ToolOutput::text(format!("sent message {id} to agent {to}")),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// Takes pending messages, optionally waiting for the first one.
pub struct Receive;
#[async_trait]
impl Tool for Receive {
    fn spec(&self) -> ToolSpec {
        spec(
            "receive",
            "Return the messages waiting for you and remove them from your mailbox. If none is waiting, wait up to yield_after seconds for the first one. Messages you do not receive this way arrive at the start of your next turn.",
            json!({"yield_after":{"type":"number","description":"Seconds to wait when no message is waiting. Default 0."}}),
            json!([]),
        )
    }
    fn effect(&self) -> Effect {
        // Taking messages changes the mailbox, so a started call runs to completion.
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let yield_after = match seconds(&input, "yield_after") {
            Ok(yield_after) => yield_after.unwrap_or_default(),
            Err(error) => return ToolOutput::error(error),
        };
        match cx.node.receive(yield_after).await {
            Ok(messages) if messages.is_empty() => ToolOutput::text("no messages"),
            Ok(messages) => ToolOutput::text(
                messages
                    .iter()
                    .map(|message| cx.node.render(message))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            ),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// Blocks until sub-agents finish and returns their results.
pub struct Wait;
#[async_trait]
impl Tool for Wait {
    fn spec(&self) -> ToolSpec {
        spec(
            "wait",
            "Wait until sub-agents finish and return their results. Without agents, wait for every sub-agent whose result you have not received yet. With timeout, return after that many seconds and list the ones still running.",
            json!({"agents":{"type":"array","items":{"type":"string"},"description":"Ids or names of your sub-agents."},"timeout":{"type":"number","description":"Seconds to wait at most."}}),
            json!([]),
        )
    }
    fn effect(&self) -> Effect {
        // Results returned here are taken from the mailbox, as with receive.
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let timeout = match seconds(&input, "timeout") {
            Ok(timeout) => timeout,
            Err(error) => return ToolOutput::error(error),
        };
        let agents = match input["agents"].as_array() {
            Some(agents) => match agents
                .iter()
                .map(|agent| cx.node.resolve(agent.as_str().unwrap_or_default()))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(agents) => Some(agents),
                Err(error) => return ToolOutput::error(error.to_string()),
            },
            None => None,
        };
        let waited = match cx.node.wait(agents.as_deref(), timeout).await {
            Ok(waited) => waited,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let mut parts = waited
            .finished
            .iter()
            .map(|outcome| cx.node.render_outcome(outcome))
            .collect::<Vec<_>>();
        if !waited.running.is_empty() {
            parts.push(format!(
                "still running: {}",
                waited
                    .running
                    .iter()
                    .map(|id| format!("agent {id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if parts.is_empty() {
            return ToolOutput::text("no sub-agents to wait for");
        }
        ToolOutput::text(parts.join("\n\n"))
    }
}
