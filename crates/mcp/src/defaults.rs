//! Defaults for MCP server connections.
use std::time::Duration;

/// Time allowed for spawn or connect, the initialize handshake and the full tool listing.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed for one tool call before it is cancelled.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Wait for a stdio server to exit after its stdin closes, before SIGTERM.
pub const EXIT_GRACE: Duration = Duration::from_secs(2);
/// Wait after SIGTERM before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(2);
/// Bound on sending a cancellation notice; the call itself has already ended.
pub const CANCEL_NOTICE: Duration = Duration::from_secs(1);
/// Bytes of server stderr kept for startup diagnostics.
pub const STDERR_TAIL_BYTES: usize = 2048;
/// Largest SSE event accepted from an HTTP server.
pub const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on tools/list pages, against servers that never stop paging.
pub const MAX_LIST_PAGES: usize = 100;
/// Provider limit on tool names (Anthropic and OpenAI both allow 64 characters).
pub const MAX_TOOL_NAME: usize = 64;
/// Longest accepted server name, so every tool keeps a readable suffix.
pub const MAX_SERVER_NAME: usize = 32;
/// Environment names a stdio server inherits by default, plus the LC_ prefix.
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR", "TZ",
];
