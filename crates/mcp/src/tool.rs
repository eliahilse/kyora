//! MCP tools as kyora tools: names, effects and result text.
use crate::{defaults, server::Connection};
use async_trait::async_trait;
use kyora_core::{Effect, Tool, ToolCx, ToolOutput};
use kyora_protocol::ToolSpec;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::time::Instant;

/// One tool of one server, exposed under its namespaced name.
pub struct McpTool {
    connection: Arc<Connection>,
    remote: String,
    spec: ToolSpec,
    effect: Effect,
}

impl McpTool {
    pub(crate) fn new(connection: Arc<Connection>, tool: rmcp::model::Tool) -> Self {
        // Servers mark side-effect-free tools with readOnlyHint. Everything else is
        // treated as a mutation, so a started call is awaited until it reports back.
        let read_only = tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            == Some(true);
        let mut input_schema = Value::Object(tool.input_schema.as_ref().clone());
        if input_schema.get("type").is_none() {
            input_schema["type"] = json!("object");
        }
        let description = tool
            .description
            .map(|description| description.into_owned())
            .or(tool.title)
            .unwrap_or_default();
        // Descriptions reach the model, so they get the same redaction as results.
        let description = connection.redact(&description);
        Self {
            spec: ToolSpec {
                name: tool_name(connection.server(), &tool.name),
                description,
                input_schema,
                large_input: false,
            },
            remote: tool.name.into_owned(),
            effect: if read_only {
                Effect::ReadOnly
            } else {
                Effect::Mutating
            },
            connection,
        }
    }

    /// The model-visible, namespaced name.
    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The tool's name on its server.
    pub fn remote_name(&self) -> &str {
        &self.remote
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    /// The server validates inputs against its full JSON Schema; the runtime's
    /// subset would reject valid inputs such as null types or type unions.
    fn validate_locally(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let deadline = (Instant::now() + self.connection.timeout()).min(cx.node.deadline);
        self.connection
            .call(&self.remote, input, &cx.cancel, deadline)
            .await
    }
}

/// The model-visible name `mcp__<server>__<tool>`, limited to `[A-Za-z0-9_-]` and
/// 64 characters. When the tool name had to be changed or shortened, a hash of the
/// original name is appended, so distinct server tools keep distinct names.
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("mcp__{server}__{clean}");
    if clean == tool && !tool.is_empty() && name.len() <= defaults::MAX_TOOL_NAME {
        return name;
    }
    let hash: String = Sha256::digest(tool.as_bytes())
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let room = defaults::MAX_TOOL_NAME
        .saturating_sub("mcp__".len() + server.len() + "__".len() + 1 + hash.len());
    let head: String = clean.chars().take(room).collect();
    format!("mcp__{server}__{head}_{hash}")
}

/// Renders a `tools/call` result as text. Images, audio and binary resources are
/// described rather than inlined; `isError` becomes an error the model sees.
pub fn render_result(result: &Value) -> ToolOutput {
    let mut parts = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        let text = |field: &str| block[field].as_str().unwrap_or_default();
        parts.push(match text("type") {
            "text" => text("text").to_owned(),
            kind @ ("image" | "audio") => format!(
                "[{kind}: {}, {} bytes]",
                text("mimeType"),
                decoded_len(text("data"))
            ),
            "resource_link" => {
                let mut link = format!("[resource link: {} {}]", text("name"), text("uri"));
                if let Some(description) = block["description"].as_str() {
                    link.push('\n');
                    link.push_str(description);
                }
                link
            }
            "resource" => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                if let Some(body) = resource["text"].as_str() {
                    format!("[resource: {uri}]\n{body}")
                } else {
                    format!(
                        "[resource: {uri}, {}, {} bytes]",
                        resource["mimeType"]
                            .as_str()
                            .unwrap_or("application/octet-stream"),
                        decoded_len(resource["blob"].as_str().unwrap_or_default())
                    )
                }
            }
            other => format!("[unsupported content type: {other}]"),
        });
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(structured.to_string());
    }
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = if !parts.is_empty() {
        parts.join("\n")
    } else if is_error {
        "the tool reported an error without details".into()
    } else {
        "(no content)".into()
    };
    let mut output = ToolOutput::text(text);
    output.is_error = is_error;
    output
}

/// Size of base64 data once decoded, without decoding it.
fn decoded_len(data: &str) -> usize {
    let data = data.trim_end();
    let padding = data.bytes().rev().take_while(|&b| b == b'=').count();
    (data.len() * 3 / 4).saturating_sub(padding)
}
