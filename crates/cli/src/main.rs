//! Command-line entry point for kyora.
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kyora_core::{
    AgentSpec, Limits, ModelRef, Runtime, RuntimeConfig, Status, ToolSelection, TraceEvent,
    TraceSink, defaults,
    session::{self, SessionStore},
    trace::TraceRecord,
};
use kyora_protocol::{Effort, RequestOptions, ThinkingDisplay};
use kyora_providers::{ModelProvider, RetryPolicy, fake::ScriptedProvider};
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use tokio::sync::{broadcast, oneshot};

#[derive(Parser)]
#[command(name = "kyora", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Open the interactive terminal UI (offline prototype).
    Tui {
        /// Play a scripted recursive run immediately.
        #[arg(long)]
        demo: bool,
    },
    /// Run a task in the workspace.
    Run(Box<Run>),
    /// List recorded sessions.
    Sessions {
        #[arg(long)]
        json: bool,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Reasoning {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}
impl From<Reasoning> for Effort {
    fn from(value: Reasoning) -> Self {
        match value {
            Reasoning::Low => Self::Low,
            Reasoning::Medium => Self::Medium,
            Reasoning::High => Self::High,
            Reasoning::Xhigh => Self::Xhigh,
            Reasoning::Max => Self::Max,
        }
    }
}
#[derive(Parser)]
struct Run {
    task: String,
    #[arg(short, long, env = "KYORA_MODEL", default_value = defaults::DEFAULT_MODEL)]
    model: ModelRef,
    #[arg(long, env = "KYORA_LLM_MODEL", default_value = defaults::DEFAULT_LLM_MODEL)]
    llm_model: ModelRef,
    #[arg(long, value_enum)]
    effort: Option<Reasoning>,
    #[arg(long)]
    budget: Option<u64>,
    #[arg(long)]
    max_turns: Option<u32>,
    #[arg(long)]
    max_depth: Option<u32>,
    #[arg(long)]
    max_agents: Option<u32>,
    #[arg(long)]
    max_live_agents: Option<u32>,
    #[arg(long, value_parser = humantime::parse_duration)]
    timeout: Option<Duration>,
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,
    #[arg(short = 'C', long = "cd")]
    cwd: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    quiet: bool,
    #[arg(long)]
    show_thinking: bool,
    #[arg(long)]
    no_session: bool,
    #[arg(long, hide = true, env = "KYORA_FAKE_SCRIPT")]
    fake_script: Option<PathBuf>,
}
/// Single wiring point for the future Anthropic implementation.
fn provider(script: Option<PathBuf>) -> Result<Arc<dyn ModelProvider>> {
    match script {
        Some(path) => Ok(Arc::new(ScriptedProvider::from_json(
            &std::fs::read_to_string(path).context("read fake script")?,
        )?)),
        None => bail!("the anthropic provider is not wired in yet"),
    }
}
fn main() -> ExitCode {
    ExitCode::from(with_runtime(dispatch(Cli::parse())))
}
fn with_runtime(work: impl std::future::Future<Output = u8>) -> u8 {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create runtime");
    let code = runtime.block_on(work);
    // Detached read-only blocking work must not hold the CLI open after trace flush.
    runtime.shutdown_background();
    code
}
async fn dispatch(cli: Cli) -> u8 {
    match cli.command {
        None | Some(Command::Tui { demo: false }) => tui(false).await,
        Some(Command::Tui { demo: true }) => tui(true).await,
        Some(Command::Run(run)) => execute(*run).await,
        Some(Command::Sessions { json }) => match defaults::home(None)
            .and_then(|home| session::list(&home))
        {
            Ok(sessions) => {
                for session in sessions {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&session).expect("serializable session")
                        );
                    } else {
                        println!(
                            "{}\t{}\t{}\t{}",
                            session.id, session.start_time, session.status, session.task_preview
                        );
                    }
                }
                0
            }
            Err(error) => {
                eprintln!("error: {error:#}");
                1
            }
        },
    }
}
async fn tui(demo: bool) -> u8 {
    match kyora_tui::run(demo).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}
async fn execute(run: Run) -> u8 {
    let prepared = prepare(&run);
    let (limits, cwd, toolset) = match prepared {
        Ok(p) => p,
        Err(error) => {
            eprintln!("error: {error:#}");
            return 2;
        }
    };
    let result = execute_runtime(run, limits, cwd, toolset).await;
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    }
}
fn prepare(run: &Run) -> Result<(Limits, PathBuf, kyora_core::Toolset)> {
    let mut limits = Limits::default();
    if let Some(v) = run.budget {
        limits.budget_tokens = v;
    }
    if let Some(v) = run.max_turns {
        limits.max_turns = v;
    }
    if let Some(v) = run.max_depth {
        limits.max_depth = v;
    }
    if let Some(v) = run.max_agents {
        limits.max_agents_total = v;
    }
    if let Some(v) = run.max_live_agents {
        limits.max_agents_live = v;
    }
    if let Some(v) = run.timeout {
        limits.run_timeout = v;
    }
    limits.validate()?;
    let current_dir = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(run.cwd.as_ref().unwrap_or(&current_dir))
        .context("working directory")?;
    if !cwd.is_dir() {
        bail!("working directory is not a directory");
    }
    let pwd = std::env::var_os("PWD").map(PathBuf::from);
    let files = kyora_tools::defaults::FileConfig {
        root_aliases: kyora_tools::workspace_root_aliases(
            &cwd,
            run.cwd.as_deref(),
            &current_dir,
            pwd.as_deref(),
        ),
        ..kyora_tools::defaults::FileConfig::default()
    };
    let tools = kyora_tools::toolset(kyora_tools::ShellConfig::default(), files)?
        .select(&ToolSelection(run.tools.clone()))?;
    Ok((limits, cwd, tools))
}
async fn execute_runtime(
    run: Run,
    limits: Limits,
    cwd: PathBuf,
    tools: kyora_core::Toolset,
) -> Result<u8> {
    let backend = provider(run.fake_script)?;
    let mut providers = BTreeMap::new();
    providers.insert(run.model.provider.clone(), backend.clone());
    providers.insert(run.llm_model.provider.clone(), backend);
    let store = if run.no_session {
        None
    } else {
        Some(SessionStore::create(&defaults::home(None)?)?)
    };
    let trace = store
        .as_ref()
        .map_or_else(TraceSink::ephemeral, |s| s.trace.clone());
    let session = store.as_ref().map_or_else(uuid_id, |s| s.id.clone());
    let runtime = Runtime::new(RuntimeConfig {
        providers,
        toolsets: Arc::new(tools),
        limits,
        retry: RetryPolicy::default(),
        llm_model: run.llm_model,
        trace: trace.clone(),
        session,
    })?;
    let receiver = trace.subscribe();
    let (stop, stopped) = oneshot::channel();
    let renderer = tokio::spawn(render(
        receiver,
        stopped,
        run.json,
        run.quiet,
        run.show_thinking,
    ));
    let control = runtime.clone();
    let interrupt = tokio::spawn(async move {
        let mut last = None;
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            let now = tokio::time::Instant::now();
            control.cancel();
            if last.is_some_and(|then| now.duration_since(then) <= defaults::INTERRUPT_WINDOW) {
                kyora_tools::cancel_processes();
                std::process::exit(130);
            }
            last = Some(now);
        }
    });
    let mut spec = AgentSpec::new(run.task, cwd);
    spec.model = run.model;
    spec.options = RequestOptions {
        effort: run.effort.map(Into::into),
        thinking_display: Some(if run.show_thinking {
            ThinkingDisplay::Summarized
        } else {
            ThinkingDisplay::Omitted
        }),
        ..RequestOptions::default()
    };
    let outcome = runtime.run(spec).await;
    interrupt.abort();
    let _ = interrupt.await;
    let flushed = trace.finish().await;
    let _ = stop.send(());
    renderer.await??;
    flushed?;
    let outcome = outcome?;
    if !run.json {
        println!("{}", outcome.answer.text());
    }
    Ok(match outcome.status {
        Status::Completed => 0,
        Status::Failed => 1,
        Status::MaxTurns | Status::BudgetExhausted | Status::Timeout | Status::ContextExhausted => {
            3
        }
        Status::Refused => 4,
        Status::Cancelled | Status::Interrupted => 130,
    })
}
fn uuid_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
async fn render(
    mut receiver: broadcast::Receiver<TraceRecord>,
    mut stop: oneshot::Receiver<()>,
    json: bool,
    quiet: bool,
    thinking: bool,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            record = receiver.recv() => match record {
                Ok(record) => display(record,json,quiet,thinking)?,
                Err(broadcast::error::RecvError::Lagged(count)) => { if json { bail!("trace subscriber lagged by {count} events"); } },
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = &mut stop => { while let Ok(record) = receiver.try_recv() { display(record,json,quiet,thinking)?; } break; },
        }
    }
    Ok(())
}
fn display(record: TraceRecord, json: bool, quiet: bool, thinking: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&record)?);
        return Ok(());
    }
    match record.event {
        TraceEvent::NodeStart {
            node, depth, kind, ..
        } if !quiet => eprintln!("{}#{node} {kind} start", "  ".repeat(depth as usize)),
        TraceEvent::NodeEnd { outcome } if !quiet => eprintln!(
            "#{} end {}",
            outcome.node,
            serde_json::to_value(outcome.status)?
                .as_str()
                .unwrap_or("failed")
        ),
        TraceEvent::ToolCall { node, name, .. } if !quiet => eprintln!("#{node} tool {name}"),
        TraceEvent::Delta {
            event: kyora_protocol::StreamEvent::ThinkingDelta { thinking: text, .. },
            ..
        } if thinking => eprint!("{text}"),
        TraceEvent::Error { message, .. } => eprintln!("error: {message}"),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_shutdown_exits_after_session_end_with_a_blocked_read() {
        const CHILD: &str = "KYORA_TEST_BLOCKED_READ";
        if std::env::var_os(CHILD).is_some() {
            let code = with_runtime(async {
                let (entered, ready) = oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    loop {
                        std::thread::park();
                    }
                });
                ready.await.unwrap();
                let store = SessionStore::create(&defaults::home(None).unwrap()).unwrap();
                store
                    .trace
                    .emit(TraceEvent::SessionEnd {
                        status: Status::Timeout,
                    })
                    .await
                    .unwrap();
                store.trace.finish().await.unwrap();
                3
            });
            assert_eq!(code, 3);
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::runtime_shutdown_exits_after_session_end_with_a_blocked_read",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("KYORA_HOME", home.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("blocking read held the process open after session_end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let session = std::fs::read_dir(home.path().join("sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let records = std::fs::read_to_string(session.join("events.jsonl")).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(records.lines().last().unwrap()).unwrap();
        assert_eq!(event["type"], "session_end");
        assert_eq!(event["status"], "timeout");
    }
}
