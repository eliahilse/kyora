use crate::defaults;
use async_trait::async_trait;
use kyora_core::{Effect, Tool, ToolCx, ToolOutput};
use kyora_protocol::ToolSpec;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Configurable shell process and bounded capture settings.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Wall-clock timeout, overridable per tool call.
    pub timeout: Duration,
    /// Maximum bytes captured per pipe.
    pub output_bytes: usize,
    /// Extra environment names allowed, subject to credential scrubbing.
    pub env_extras: Vec<String>,
    /// Shell paths tried in order, each invoked with -c.
    pub shells: Vec<PathBuf>,
}
impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout: defaults::SHELL_TIMEOUT,
            output_bytes: defaults::SHELL_OUTPUT_BYTES,
            env_extras: vec![],
            shells: defaults::SHELLS.iter().map(PathBuf::from).collect(),
        }
    }
}
/// Runs commands in an owned process group with a scrubbed environment.
pub struct Shell {
    config: ShellConfig,
}
impl Shell {
    /// Creates a shell with configurable bounds and environment extras.
    pub fn new(config: ShellConfig) -> Self {
        Self { config }
    }
}
static GROUPS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<i32>>> =
    std::sync::OnceLock::new();
fn groups() -> &'static std::sync::Mutex<std::collections::BTreeSet<i32>> {
    GROUPS.get_or_init(Default::default)
}
/// Immediately kills all managed shell groups for emergency process shutdown.
/// Normal cancellation also reaps the shell and drains its pipes.
pub fn cancel_processes() {
    for &pid in groups().lock().expect("process registry poisoned").iter() {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}
struct ProcessGroup(Pid);
impl ProcessGroup {
    fn new(pid: Pid) -> Self {
        groups()
            .lock()
            .expect("process registry poisoned")
            .insert(pid.as_raw());
        Self(pid)
    }
    fn kill(&self) {
        let _ = killpg(self.0, Signal::SIGKILL);
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
        groups()
            .lock()
            .expect("process registry poisoned")
            .remove(&self.0.as_raw());
    }
}
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    cap: usize,
    total: u64,
}
impl Capture {
    fn new(cap: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            cap,
            total: 0,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let head_cap = self.cap.div_ceil(2);
        let tail_cap = self.cap / 2;
        for &byte in bytes {
            if self.head.len() < head_cap {
                self.head.push(byte);
            } else if tail_cap > 0 {
                if self.tail.len() == tail_cap {
                    self.tail.pop_front();
                }
                self.tail.push_back(byte);
            }
        }
    }
    fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        let omitted = self
            .total
            .saturating_sub((self.head.len() + self.tail.len()) as u64);
        if omitted > 0 {
            text.push_str(&format!("[... {omitted} bytes omitted ...]"));
        }
        text.push_str(&String::from_utf8_lossy(
            &self.tail.iter().copied().collect::<Vec<_>>(),
        ));
        text
    }
}
async fn drain(
    mut pipe: impl AsyncRead + Unpin,
    cap: usize,
    deadline: tokio::time::Instant,
    cancel: tokio_util::sync::CancellationToken,
) -> std::io::Result<Capture> {
    let mut capture = Capture::new(cap);
    let mut chunk = vec![0; defaults::DRAIN_CHUNK_BYTES];
    loop {
        let read = tokio::select! {
            read = pipe.read(&mut chunk) => read?,
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep_until(deadline) => break,
        };
        if read == 0 {
            break;
        }
        capture.push(&chunk[..read]);
    }
    Ok(capture)
}
fn credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}
#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell".into(),
            description: "Run a shell command in the workspace with bounded stdout and stderr."
                .into(),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"},"timeout_s":{"type":"number"}},"required":["command"],"additionalProperties":false}),
            large_input: false,
        }
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        match self.execute(input, cx).await {
            Ok(output) => output,
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}
impl Shell {
    async fn execute(&self, input: Value, cx: ToolCx) -> anyhow::Result<ToolOutput> {
        let timeout = if let Some(value) = input.get("timeout_s") {
            let seconds = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("invalid timeout"))?;
            if !seconds.is_finite() || seconds <= 0.0 {
                anyhow::bail!("timeout must be positive");
            }
            Duration::try_from_secs_f64(seconds)?
        } else {
            self.config.timeout
        };
        if timeout.is_zero() || self.config.output_bytes == 0 {
            anyhow::bail!("shell bounds must be positive");
        }
        if cx.cancel.is_cancelled() {
            return Ok(ToolOutput::error("cancelled"));
        }
        let command = input["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing command"))?;
        let mut child = None;
        let mut last = None;
        for shell in &self.config.shells {
            let mut cmd = Command::new(shell);
            cmd.arg("-c")
                .arg(command)
                .current_dir(&cx.cwd)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (name, value) in std::env::vars_os() {
                let key = name.to_string_lossy();
                if !credential(&key)
                    && (defaults::ENV_ALLOWLIST.contains(&key.as_ref())
                        || key.starts_with("LC_")
                        || self.config.env_extras.iter().any(|n| n == &key))
                {
                    cmd.env(name, value);
                }
            }
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
            match cmd.spawn() {
                Ok(process) => {
                    child = Some(process);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => last = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        let mut child =
            child.ok_or_else(|| anyhow::anyhow!("no shell executable available: {last:?}"))?;
        let group = ProcessGroup::new(Pid::from_raw(i32::try_from(
            child
                .id()
                .ok_or_else(|| anyhow::anyhow!("missing child pid"))?,
        )?));
        // Pumps live in this future. Dropping it kills the group and drops both pipes.
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("timeout is too large"))?
            .min(cx.node.deadline);
        let stdout = drain(
            child.stdout.take().expect("piped stdout"),
            self.config.output_bytes,
            deadline,
            cx.cancel.clone(),
        );
        let stderr = drain(
            child.stderr.take().expect("piped stderr"),
            self.config.output_bytes,
            deadline,
            cx.cancel.clone(),
        );
        let wait = async {
            tokio::select! {
                biased;
                _ = cx.cancel.cancelled() => { group.kill(); child.wait().await.map(|s| (s, Some("cancelled"))) },
                _ = tokio::time::sleep_until(deadline) => { group.kill(); child.wait().await.map(|s| (s, Some("timeout"))) },
                status = child.wait() => { group.kill(); status.map(|s| (s, None)) },
            }
        };
        let (stdout, stderr, status) = tokio::join!(stdout, stderr, wait);
        let (status, reason) = status?;
        let result = json!({"stdout":stdout?.text(),"stderr":stderr?.text(),"exit_code":status.code(),"error":reason});
        let mut output = ToolOutput::text(result.to_string());
        output.is_error = reason.is_some() || !status.success();
        Ok(output)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_capture_retains_head_and_tail() {
        let mut c = Capture::new(10);
        for _ in 0..10_000 {
            c.push(b"0123456789");
            assert!(c.head.len() + c.tail.len() <= 10);
        }
        assert_eq!(c.total, 100_000);
        assert!(c.text().starts_with("01234"));
        assert!(c.text().ends_with("56789"));
    }
    #[test]
    fn all_credentials_are_denied_even_in_extras() {
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GH_TOKEN",
            "CUSTOM_PASSWORD",
        ] {
            assert!(credential(key));
        }
        assert!(!credential("PATH"));
    }
}
