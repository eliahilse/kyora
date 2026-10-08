//! Stdio server processes: scrubbed environment, an owned process group and a bounded
//! stderr tail for diagnostics.
use crate::{
    config::{ServerConfig, credential},
    defaults,
    secrets::Secrets,
};
use anyhow::{Context, Result, anyhow};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{self, Poll, ready},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    process::{Child, ChildStdin, ChildStdout, Command},
    task::JoinHandle,
};

static GROUPS: OnceLock<Mutex<BTreeSet<i32>>> = OnceLock::new();
fn groups() -> &'static Mutex<BTreeSet<i32>> {
    GROUPS.get_or_init(Default::default)
}

/// Immediately kills the process group of every running stdio server, for emergency
/// exits that skip [`crate::Servers::shutdown`].
pub fn kill_servers() {
    for &pid in groups().lock().expect("server registry poisoned").iter() {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

/// A spawned stdio server. Dropping it kills its whole process group.
pub(crate) struct Process {
    child: Child,
    group: Pid,
    stderr: Arc<Mutex<Tail>>,
    slack: usize,
    drain: Option<JoinHandle<()>>,
}

/// The last stderr bytes, and whether earlier ones were discarded.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    cut: bool,
}

impl Process {
    /// Spawns `command` in its own process group with piped stdio.
    pub(crate) fn spawn(
        config: &ServerConfig,
        command: &str,
        cwd: &Path,
        env: &[(OsString, OsString)],
        oversized: Arc<AtomicBool>,
        secrets: &Secrets,
    ) -> Result<(Self, BoundedLines<ChildStdout>, ChildStdin)> {
        let mut cmd = Command::new(command);
        cmd.args(&config.args)
            .current_dir(cwd)
            .env_clear()
            .envs(child_env(config, env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        use std::os::unix::process::CommandExt;
        // Terminal signals reach kyora only; servers are stopped through shutdown.
        cmd.as_std_mut().process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("spawn {command}"))?;
        let pid = child.id().ok_or_else(|| anyhow!("missing child pid"))?;
        let group = Pid::from_raw(i32::try_from(pid)?);
        groups()
            .lock()
            .expect("server registry poisoned")
            .insert(group.as_raw());
        let stdout = BoundedLines {
            inner: child.stdout.take().expect("piped stdout"),
            line: 0,
            exceeded: oversized,
        };
        let stdin = child.stdin.take().expect("piped stdin");
        let mut pipe = child.stderr.take().expect("piped stderr");
        let stderr = Arc::new(Mutex::new(Tail::default()));
        let tail = stderr.clone();
        // Room in front of the tail for the longest credential value, so a value cut
        // by the ring's start can still be recognized and dropped.
        let slack = secrets.reach();
        let capacity = defaults::STDERR_TAIL_BYTES + slack;
        let drain = tokio::spawn(async move {
            let mut chunk = [0; 1024];
            while let Ok(read @ 1..) = pipe.read(&mut chunk).await {
                let mut tail = tail.lock().expect("stderr tail poisoned");
                tail.bytes.extend_from_slice(&chunk[..read]);
                let excess = tail.bytes.len().saturating_sub(capacity);
                if excess > 0 {
                    tail.bytes.drain(..excess);
                    tail.cut = true;
                }
            }
        });
        let process = Self {
            child,
            group,
            stderr,
            slack,
            drain: Some(drain),
        };
        Ok((process, stdout, stdin))
    }

    /// Waits for a voluntary exit after stdin has closed, then sends SIGTERM and
    /// finally SIGKILL to the group.
    pub(crate) async fn stop(mut self) {
        if tokio::time::timeout(defaults::EXIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = killpg(self.group, Signal::SIGTERM);
            if tokio::time::timeout(defaults::TERM_GRACE, self.child.wait())
                .await
                .is_err()
            {
                let _ = killpg(self.group, Signal::SIGKILL);
                let _ = self.child.wait().await;
            }
        }
        // Drop kills anything the server left behind in its group.
    }

    /// Kills the group and returns the last stderr output, redacted, for startup
    /// failures.
    pub(crate) async fn kill(mut self, secrets: &Secrets) -> String {
        let _ = killpg(self.group, Signal::SIGKILL);
        let _ = self.child.wait().await;
        if let Some(drain) = self.drain.take() {
            // Every writer is gone, so the drain reaches end of file promptly.
            let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;
        }
        let tail = self.stderr.lock().expect("stderr tail poisoned");
        // After a cut, skip a split character so byte offsets stay aligned.
        let start = match tail.cut {
            true => tail
                .bytes
                .iter()
                .take(3)
                .take_while(|&&byte| (0x80..0xC0).contains(&byte))
                .count(),
            false => 0,
        };
        let text = String::from_utf8_lossy(&tail.bytes[start..]);
        // Redact before cutting: the slack may start inside a value.
        let from = if tail.cut {
            self.slack.saturating_sub(start)
        } else {
            0
        };
        secrets.redact_from(&text, from).trim().to_owned()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = killpg(self.group, Signal::SIGKILL);
        groups()
            .lock()
            .expect("server registry poisoned")
            .remove(&self.group.as_raw());
    }
}

/// Server stdout that fails once one line outgrows [`defaults::MAX_MESSAGE_BYTES`], so
/// a server cannot make the line reader buffer without bound.
pub(crate) struct BoundedLines<R> {
    inner: R,
    line: usize,
    exceeded: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLines<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let start = buf.filled().len();
        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        let fresh = &buf.filled()[start..];
        let longest = match (
            fresh.iter().position(|&b| b == b'\n'),
            fresh.iter().rposition(|&b| b == b'\n'),
        ) {
            (Some(first), Some(last)) => {
                let longest = this.line + first;
                this.line = fresh.len() - last - 1;
                longest.max(this.line)
            }
            _ => {
                this.line += fresh.len();
                this.line
            }
        };
        if longest > defaults::MAX_MESSAGE_BYTES {
            this.exceeded.store(true, Ordering::SeqCst);
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message exceeds the size limit",
            )));
        }
        Poll::Ready(Ok(()))
    }
}

/// The allowlist minus credential-looking names, plus `env_vars`, then literal `env`.
fn child_env(config: &ServerConfig, env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let mut vars: Vec<_> = env
        .iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            let inherited = !credential(&name)
                && (defaults::ENV_ALLOWLIST.contains(&name.as_ref()) || name.starts_with("LC_"));
            inherited || config.env_vars.iter().any(|wanted| *wanted == name)
        })
        .cloned()
        .collect();
    vars.extend(
        config
            .env
            .iter()
            .map(|(name, value)| (name.into(), value.into())),
    );
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_env_keeps_the_allowlist_and_forwards_only_named_credentials() {
        let env: Vec<(OsString, OsString)> = [
            ("PATH", "/bin"),
            ("LC_ALL", "C"),
            ("ANTHROPIC_API_KEY", "secret"),
            ("GITHUB_TOKEN", "forwarded"),
            ("UNRELATED", "dropped"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let config = ServerConfig {
            command: Some("server".into()),
            env_vars: vec!["GITHUB_TOKEN".into()],
            env: [("MODE".to_owned(), "fast".to_owned())].into(),
            ..ServerConfig::default()
        };
        let vars = child_env(&config, &env);
        let names: Vec<_> = vars.iter().map(|(k, _)| k.to_string_lossy()).collect();
        assert_eq!(names, ["PATH", "LC_ALL", "GITHUB_TOKEN", "MODE"]);
    }
}
