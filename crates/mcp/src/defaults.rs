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
/// Bound on deleting an HTTP session that startup or shutdown left open.
pub const DELETE_TIMEOUT: Duration = Duration::from_secs(5);
/// Shortest credential value that is redacted; shorter values would shred output.
pub const MIN_SECRET_CHARS: usize = 6;
/// Longest credential value accepted, so one always fits whole in a capped error body
/// or stderr tail and can be redacted there.
pub const MAX_SECRET_BYTES: usize = 4096;
/// Bytes of server stderr kept for startup diagnostics.
pub const STDERR_TAIL_BYTES: usize = 2048;
/// Largest single message accepted from a server: a stdio line, an HTTP JSON body or
/// an SSE event.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of an HTTP error body read for diagnostics.
pub const ERROR_BODY_BYTES: usize = 64 * 1024;
/// Upper bound on tools/list pages, against servers that never stop paging.
pub const MAX_LIST_PAGES: usize = 100;
/// Most tools one server may offer.
pub const MAX_TOOLS: usize = 1024;
/// Most bytes of serialized tool definitions one server may offer.
pub const MAX_LISTING_BYTES: usize = 8 * 1024 * 1024;
/// Provider limit on tool names (Anthropic and OpenAI both allow 64 characters).
pub const MAX_TOOL_NAME: usize = 64;
/// Longest accepted server name, so every tool keeps a readable suffix.
pub const MAX_SERVER_NAME: usize = 32;
/// Environment names a stdio server inherits by default, plus the LC_ prefix.
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR", "TZ",
];
