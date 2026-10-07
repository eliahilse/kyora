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

/// Default frozen child prompt, shared byte-for-byte by siblings.
/// Override with Runtime::set_subagent_prompt before starting the root.
pub const SUBAGENT: &str = "You are a sub-agent working on the supplied task in the current working directory. Use the available tool schemas to supply arguments. Return the final answer as text.";
