//! Product defaults and validated runtime limits.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

/// Default root agent model. Sub-agents inherit their parent's model.
pub const DEFAULT_MODEL: &str = "anthropic/claude-opus-5-5";
/// Default leaf completion model.
pub const DEFAULT_LLM_MODEL: &str = "anthropic/claude-sonnet-5-5";
/// Provider used by bare model names.
pub const DEFAULT_PROVIDER: &str = "anthropic";
/// Session directory relative to HOME.
pub const HOME_DIRECTORY: &str = ".kyora";
/// Payload size above which non-message trace payloads use blobs.
pub const BLOB_THRESHOLD: usize = 64 * 1024;
/// Smallest output cap allowed when reducing a reservation.
pub const MIN_OUTPUT_TOKENS: u32 = 4096;
/// Default task-preview length in session listings.
pub const SESSION_PREVIEW_CHARS: usize = 120;
/// Broadcast buffer capacity; subscribers must handle lag.
pub const TRACE_CAPACITY: usize = 4096;
/// Writer queue capacity, bounded independently of the broadcast channel.
pub const WRITER_CAPACITY: usize = 256;
/// Interval in which a second interrupt exits immediately.
pub const INTERRUPT_WINDOW: Duration = Duration::from_secs(2);

/// Time allowed for a cancelled provider to report whether it sent the request.
pub const PROVIDER_CANCEL_GRACE: Duration = Duration::from_millis(250);
/// Default number of undelivered plain messages one agent's mailbox holds.
pub const MAILBOX_CAPACITY: u32 = 64;
/// Default message body cap in characters.
pub const MESSAGE_CHARS: usize = 20_000;
/// Default character budget of one delivery of messages to a model.
pub const DELIVERY_CHARS: usize = 60_000;

/// Tree and request limits. All counts and durations must be positive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Maximum agent recursion depth (root is zero).
    pub max_depth: u32,
    /// Processed-token budget, including cache reads.
    pub budget_tokens: u64,
    /// Maximum admitted agents, including the root.
    pub max_agents_total: u32,
    /// Maximum agents alive at once, including the root.
    pub max_agents_live: u32,
    /// Maximum admitted leaf completions.
    pub max_llm_calls: u32,
    /// Maximum unsettled model attempts.
    pub max_inflight_requests: u32,
    /// Root agent turn cap.
    pub max_turns: u32,
    /// Default child agent turn cap.
    pub subagent_max_turns: u32,
    /// Root wall-clock deadline.
    pub run_timeout: Duration,
    /// Default REPL cell wall-clock deadline.
    pub cell_timeout: Duration,
    /// Upper bound on an overridden cell deadline.
    pub max_cell_timeout: Duration,
    /// Maximum time without a model stream event.
    pub request_idle: Duration,
    /// Maximum duration of one model attempt.
    pub request_total: Duration,
    /// Agent output cap.
    pub max_output_tokens: u32,
    /// Leaf completion output cap.
    pub llm_max_output_tokens: u32,
    /// Tool result character cap.
    pub tool_output_chars: usize,
    /// Undelivered plain messages one agent's mailbox holds. Result notices of
    /// children are not counted.
    #[serde(default = "mailbox_capacity")]
    pub mailbox_capacity: u32,
    /// Message body character cap. Longer sends are refused; a child's answer in
    /// its result notice is shortened to this cap.
    #[serde(default = "message_chars")]
    pub message_chars: usize,
    /// Character budget of one delivery: a turn boundary, a receive or a wait hands
    /// over whole messages in arrival order until the next would exceed it, always
    /// at least one. The rest stays queued for the next delivery.
    #[serde(default = "delivery_chars")]
    pub delivery_chars: usize,
}
fn mailbox_capacity() -> u32 {
    MAILBOX_CAPACITY
}
fn message_chars() -> usize {
    MESSAGE_CHARS
}
fn delivery_chars() -> usize {
    DELIVERY_CHARS
}
impl Default for Limits {
    /// D10.1 defaults, configurable before constructing a runtime.
    fn default() -> Self {
        Self {
            max_depth: 2,
            budget_tokens: 20_000_000,
            max_agents_total: 100,
            max_agents_live: 16,
            max_llm_calls: 2000,
            max_inflight_requests: 16,
            max_turns: 200,
            subagent_max_turns: 50,
            run_timeout: Duration::from_secs(7200),
            cell_timeout: Duration::from_secs(1800),
            max_cell_timeout: Duration::from_secs(7200),
            request_idle: Duration::from_secs(300),
            request_total: Duration::from_secs(1800),
            max_output_tokens: 32_000,
            llm_max_output_tokens: 16_000,
            tool_output_chars: 20_000,
            mailbox_capacity: MAILBOX_CAPACITY,
            message_chars: MESSAGE_CHARS,
            delivery_chars: DELIVERY_CHARS,
        }
    }
}
impl Limits {
    /// Rejects zero counts, durations and inconsistent cell deadlines.
    pub fn validate(&self) -> Result<()> {
        if [
            u64::from(self.max_depth),
            self.budget_tokens,
            u64::from(self.max_agents_total),
            u64::from(self.max_agents_live),
            u64::from(self.max_llm_calls),
            u64::from(self.max_inflight_requests),
            u64::from(self.max_turns),
            u64::from(self.subagent_max_turns),
            u64::from(self.max_output_tokens),
            u64::from(self.llm_max_output_tokens),
            self.tool_output_chars as u64,
            u64::from(self.mailbox_capacity),
            self.message_chars as u64,
            self.delivery_chars as u64,
        ]
        .contains(&0)
        {
            bail!("limits must be positive integers");
        }
        if [
            self.run_timeout,
            self.cell_timeout,
            self.max_cell_timeout,
            self.request_idle,
            self.request_total,
        ]
        .iter()
        .any(Duration::is_zero)
        {
            bail!("durations must be positive");
        }
        if [
            self.run_timeout,
            self.cell_timeout,
            self.max_cell_timeout,
            self.request_idle,
            self.request_total,
        ]
        .iter()
        .any(|duration| std::time::Instant::now().checked_add(*duration).is_none())
        {
            bail!("duration is too large for a deadline");
        }
        if self.cell_timeout > self.max_cell_timeout {
            bail!("cell timeout exceeds maximum");
        }
        Ok(())
    }
}
/// A parsed provider/model reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Registry provider name.
    pub provider: String,
    /// Provider-local model name.
    pub model: String,
}
impl std::str::FromStr for ModelRef {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (provider, model) = value.split_once('/').unwrap_or((DEFAULT_PROVIDER, value));
        if provider.is_empty() || model.is_empty() || value.trim() != value {
            bail!("invalid model reference");
        }
        Ok(Self {
            provider: provider.into(),
            model: model.into(),
        })
    }
}
impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}
/// Resolves explicit path, KYORA_HOME, then HOME/.kyora, in that order.
pub fn home(explicit: Option<PathBuf>) -> Result<PathBuf> {
    explicit
        .or_else(|| std::env::var_os("KYORA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(HOME_DIRECTORY)))
        .ok_or_else(|| anyhow::anyhow!("HOME or KYORA_HOME must be set"))
}

/// Default child tools, intersected with the parent's frozen capabilities.
/// A ChildSpec can override this rule with an explicit ToolSelection.
pub const SUBAGENT_TOOLS: &[&str] = &[
    "python",
    "read_file",
    "spawn_agent",
    "send_message",
    "receive",
    "wait",
    "cancel_agent",
];
