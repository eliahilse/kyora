//! MCP client: tools of configured MCP servers as ordinary kyora tools.
//!
//! Servers run over stdio (a spawned process) or streamable HTTP, through the official
//! `rmcp` SDK. Each server tool becomes a [`kyora_core::Tool`] named
//! `mcp__<server>__<tool>`, so node tool selections include or exclude it like any
//! built-in tool. A server that fails to start is reported and left out; the others
//! keep running.
#![cfg(unix)]
pub mod config;
pub mod defaults;
mod http;
mod process;
mod server;
mod tool;
pub use config::{McpConfig, ServerConfig};
pub use process::kill_servers;
pub use server::{McpToolsets, Server, Servers};
pub use tool::{McpTool, render_result, tool_name};
