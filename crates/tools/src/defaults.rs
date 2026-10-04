//! Configurable defaults for built-in Unix tools.
use std::{path::PathBuf, time::Duration};
/// Trusted workspace spellings and bounds for file reads.
#[derive(Debug, Clone)]
pub struct FileConfig {
    /// Additional absolute workspace spellings, computed once from trusted inputs
    /// with [`crate::workspace_root_aliases`] (canonical root first). They are
    /// used only by nodes whose cwd canonicalizes to that first entry, and each
    /// alias is re-checked against the root when a path uses it.
    pub root_aliases: Vec<PathBuf>,
    /// Default number of lines returned.
    pub lines: usize,
    /// Maximum bytes examined, including skipped lines.
    pub max_bytes: usize,
    /// Maximum characters retained per line.
    pub line_chars: usize,
}
impl Default for FileConfig {
    /// Returns no additional root aliases, 2000 lines, a 4 MiB bounded read
    /// and 2000 characters per line. The runtime cwd is always accepted.
    fn default() -> Self {
        Self {
            root_aliases: Vec::new(),
            lines: 2000,
            max_bytes: 4 * 1024 * 1024,
            line_chars: 2000,
        }
    }
}
/// Default shell wall-clock timeout.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
/// Default head and tail capture size per output pipe, in bytes.
pub const SHELL_OUTPUT_BYTES: usize = 20_000;
/// Drain buffer size; independent of generated output volume.
pub const DRAIN_CHUNK_BYTES: usize = 8192;
/// Shell executables tried in order; overriding ShellConfig changes this selection.
pub const SHELLS: &[&str] = &["/bin/bash", "/bin/sh"];
/// Child environment names retained by default, plus the LC_ prefix.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "TERM", "TMPDIR", "USER"];
