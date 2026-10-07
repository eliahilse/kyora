//! Frozen tool registries, input validation and bounded result formatting.
use crate::{Answer, NodeCtx, TraceSink};
use anyhow::{Result, bail};
use async_trait::async_trait;
use kyora_protocol::{ToolResultPart, ToolSpec};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Tool effects used by cancellation and future parallel dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads without modifying the workspace.
    ReadOnly,
    /// May change workspace state. Runtime waits for its real outcome once started.
    Mutating,
}
/// Context passed to one tool execution.
#[derive(Clone)]
pub struct ToolCx {
    /// Owning node and recursion entry point.
    pub node: NodeCtx,
    /// Provider's tool-use identifier.
    pub call_id: String,
    /// Workspace for relative paths and commands.
    pub cwd: PathBuf,
    /// Cancellation token for this execution.
    pub cancel: CancellationToken,
    /// Shared event sender.
    pub events: TraceSink,
}
/// Tool result, optionally committing an agent answer.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text for the model.
    pub content: Vec<ToolResultPart>,
    /// Whether execution failed.
    pub is_error: bool,
    /// First committed answer wins within an assistant turn.
    pub final_answer: Option<Answer>,
}
impl ToolOutput {
    /// Concatenates the text parts for display and truncation.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .map(|part| match part {
                ToolResultPart::Text { text } => text.as_str(),
            })
            .collect()
    }
    /// Constructs a successful text result.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultPart::Text {
                text: content.into(),
            }],
            is_error: false,
            final_answer: None,
        }
    }
    /// Constructs an error text result.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultPart::Text {
                text: content.into(),
            }],
            is_error: true,
            final_answer: None,
        }
    }
}
/// Pluggable local tool. Runtime validates input before calling it.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Frozen model-visible specification.
    fn spec(&self) -> ToolSpec;
    /// Declared workspace effect.
    fn effect(&self) -> Effect;
    /// Whether providers should stream large arguments eagerly.
    fn large_input(&self) -> bool {
        self.spec().large_input
    }
    /// Whether the runtime checks inputs against the schema subset before calling.
    /// Tools whose schemas are enforced elsewhere, such as MCP tools validated by
    /// their server, return false so valid inputs outside the subset still arrive.
    fn validate_locally(&self) -> bool {
        true
    }
    /// Executes one validated call. Implementations must honor cancellation.
    /// Started mutations must finish or stop safely before returning an outcome.
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput;
}
/// Tool name restriction; None selects all factory tools.
#[derive(Debug, Clone, Default)]
pub struct ToolSelection(pub Option<Vec<String>>);
/// Immutable tool registry with specifications frozen at construction.
#[derive(Clone, Default)]
pub struct Toolset {
    entries: BTreeMap<String, (ToolSpec, Arc<dyn Tool>)>,
}
impl Toolset {
    /// Freezes tool specifications, rejecting duplicate names.
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for tool in tools {
            let mut spec = tool.spec();
            spec.large_input = tool.large_input();
            if entries.insert(spec.name.clone(), (spec, tool)).is_some() {
                bail!("duplicate tool name");
            }
        }
        Ok(Self { entries })
    }
    /// Selects a subset, rejecting unknown requested names.
    pub fn select(&self, selection: &ToolSelection) -> Result<Self> {
        let Some(names) = &selection.0 else {
            return Ok(self.clone());
        };
        let mut entries = BTreeMap::new();
        for name in names {
            entries.insert(
                name.clone(),
                self.entries
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?
                    .clone(),
            );
        }
        Ok(Self { entries })
    }
    /// Returns frozen specs in stable name order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.values().map(|(s, _)| s.clone()).collect()
    }
    /// Validates tool existence and, unless the tool opts out, its arguments.
    pub fn validate(&self, name: &str, input: &Value) -> Result<()> {
        let (spec, tool) = self
            .entries
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?;
        if !tool.validate_locally() {
            return Ok(());
        }
        validate(&spec.input_schema, input)
    }
    /// Looks up a tool after validation.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.entries.get(name).map(|(_, t)| t.clone())
    }
}
/// Builds a frozen toolset per node, independent of concrete tools in core.
pub trait ToolsetFactory: Send + Sync {
    /// Selects tools for a newly admitted node.
    fn toolset(
        &self,
        node: &crate::runtime::NodeInfo,
        selection: &ToolSelection,
    ) -> Result<Toolset>;
}
impl ToolsetFactory for Toolset {
    fn toolset(
        &self,
        _node: &crate::runtime::NodeInfo,
        selection: &ToolSelection,
    ) -> Result<Toolset> {
        self.select(selection)
    }
}
/// Validates a small JSON-schema subset, including nested properties and items.
pub fn validate(schema: &Value, input: &Value) -> Result<()> {
    let valid = match schema["type"].as_str() {
        Some("object") => input.is_object(),
        Some("array") => input.is_array(),
        Some("string") => input.is_string(),
        Some("integer") => input.is_i64() || input.is_u64(),
        Some("number") => input.is_number(),
        Some("boolean") => input.is_boolean(),
        None => true,
        Some(other) => bail!("unsupported schema type: {other}"),
    };
    if !valid {
        bail!("invalid input type, expected {}", schema["type"]);
    }
    if let Some(object) = input.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required {
                let key = key
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid required schema"))?;
                if !object.contains_key(key) {
                    bail!("missing property: {key}");
                }
            }
        }
        for (key, value) in object {
            if let Some(property) = schema["properties"].get(key) {
                validate(property, value)?;
            } else if schema["additionalProperties"] == false {
                bail!("unexpected property: {key}");
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), input.as_array()) {
        for value in array {
            validate(items, value)?;
        }
    }
    Ok(())
}
/// Keeps head and tail within a character cap, with an omission marker.
/// For caps too small to contain a marker, keeps only the head.
pub fn truncate(text: &str, cap: usize) -> String {
    let length = text.chars().count();
    if length <= cap {
        return text.into();
    }
    let mut kept = cap;
    loop {
        let marker = format!("[... {} characters omitted ...]", length - kept);
        if marker.len() > cap {
            return text.chars().take(cap).collect();
        }
        let next = cap - marker.len();
        if next >= kept {
            let head = kept.div_ceil(2);
            let tail = kept / 2;
            return text
                .chars()
                .take(head)
                .chain(marker.chars())
                .chain(text.chars().skip(length - tail))
                .collect();
        }
        kept = next;
    }
}
