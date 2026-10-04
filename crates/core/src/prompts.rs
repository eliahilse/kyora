//! Frozen, factual prompts.
/// Builds the root system prompt from frozen tool names.
pub fn root(tools: &[kyora_protocol::ToolSpec]) -> String {
    format!(
        "You are an agent working in the current working directory. Available tools: {}. Use their schemas to supply arguments. Return the final answer as text.",
        tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
