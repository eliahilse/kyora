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
use kyora_providers::anthropic::AnthropicProvider;
use kyora_providers::{ModelProvider, RetryPolicy, fake::ScriptedProvider};
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use tokio::sync::{broadcast, oneshot};

mod config;

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
#[command(
    after_help = "Settings: flags override KYORA_MODEL, KYORA_LLM_MODEL and KYORA_EFFORT, then $KYORA_HOME/config.toml, then built-in defaults.\nAnthropic credentials: configured api_key_env (default ANTHROPIC_API_KEY), then providers.anthropic.api_key. Config files containing api_key require chmod 600. Base URL: --base-url overrides ANTHROPIC_BASE_URL, then providers.anthropic.base_url, then the provider default."
)]
struct Run {
    task: String,
    /// Root model as provider/model (default: built-in root model).
    #[arg(short, long, value_name = "PROVIDER/MODEL")]
    model: Option<String>,
    /// Default model for leaf completions as provider/model.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    llm_model: Option<String>,
    /// Anthropic API origin, including an optional proxy path prefix.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,
    /// Reasoning effort (unset uses the provider default).
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
/// Single wiring point for CLI providers.
fn provider(
    name: &str,
    config: &config::Config,
    path: &std::path::Path,
    base_url: Option<&str>,
    scripted: Option<&Arc<dyn ModelProvider>>,
) -> Result<Arc<dyn ModelProvider>> {
    if let Some(scripted) = scripted {
        return Ok(scripted.clone());
    }
    match name {
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(
            config.anthropic(path, base_url)?,
        )?)),
        _ => bail!("unknown provider {name}; supported providers: anthropic"),
    }
}
struct Resolved {
    model: ModelRef,
    llm_model: ModelRef,
    effort: Option<Effort>,
}
impl Resolved {
    fn new(run: &mut Run, config: &config::Config, path: &std::path::Path) -> Result<Self> {
        let model = config::model(
            run.model.take(),
            "KYORA_MODEL",
            config.model.as_deref(),
            defaults::DEFAULT_MODEL,
            path,
            "model",
        )?;
        let llm_model = config::model(
            run.llm_model.take(),
            "KYORA_LLM_MODEL",
            config.llm_model.as_deref(),
            defaults::DEFAULT_LLM_MODEL,
            path,
            "llm_model",
        )?;
        config.reject_model_credentials(&[&model.value, &llm_model.value])?;
        let model = model.parse()?;
        let llm_model = llm_model.parse()?;
        let effort = if let Some(effort) = run.effort {
            Some(effort.into())
        } else if let Some(value) = config::env("KYORA_EFFORT")? {
            Some(
                Reasoning::from_str(&value, false)
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "invalid KYORA_EFFORT: expected low, medium, high, xhigh or max"
                        )
                    })?
                    .into(),
            )
        } else {
            config.effort.or(RequestOptions::default().effort)
        };
        if run.fake_script.is_none() {
            for name in [&model.provider, &llm_model.provider] {
                if name != "anthropic" {
                    bail!("unknown provider {name}; supported providers: anthropic");
                }
            }
        }
        Ok(Self {
            model,
            llm_model,
            effort,
        })
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
async fn execute(mut run: Run) -> u8 {
    let prepared = (|| {
        let path = defaults::home(None)?.join(config::FILE_NAME);
        let config = config::Config::load(&path)?;
        let resolved = Resolved::new(&mut run, &config, &path)?;
        Ok::<_, anyhow::Error>((prepare(&run)?, resolved, config, path))
    })();
    let ((limits, cwd, toolset), resolved, config, path) = match prepared {
        Ok(p) => p,
        Err(error) => {
            eprintln!("error: {error:#}");
            return 2;
        }
    };
    let result = execute_runtime(run, resolved, config, path, limits, cwd, toolset).await;
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
    resolved: Resolved,
    config: config::Config,
    config_path: PathBuf,
    limits: Limits,
    cwd: PathBuf,
    tools: kyora_core::Toolset,
) -> Result<u8> {
    let scripted: Option<Arc<dyn ModelProvider>> = run
        .fake_script
        .as_ref()
        .map(|path| {
            Ok::<_, anyhow::Error>(Arc::new(ScriptedProvider::from_json(
                &std::fs::read_to_string(path).context("read fake script")?,
            )?) as Arc<dyn ModelProvider>)
        })
        .transpose()?;
    let mut providers = BTreeMap::new();
    for name in [&resolved.model.provider, &resolved.llm_model.provider] {
        if !providers.contains_key(name) {
            providers.insert(
                name.clone(),
                provider(
                    name,
                    &config,
                    &config_path,
                    run.base_url.as_deref(),
                    scripted.as_ref(),
                )?,
            );
        }
    }
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
        llm_model: resolved.llm_model,
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
    spec.model = resolved.model;
    spec.options = RequestOptions {
        effort: resolved.effort,
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
    fn config_resolution_precedence() {
        const CHILD: &str = "KYORA_TEST_CONFIG_LAYER";
        if let Ok(layer) = std::env::var(CHILD) {
            let config = if layer == "default" {
                config::Config::default()
            } else {
                toml::from_str("model = 'anthropic/config-root'\nllm_model = 'anthropic/config-leaf'\neffort = 'low'\n[providers.anthropic]\nbase_url = 'http://127.0.0.1/config'").unwrap()
            };
            let mut args = vec!["kyora", "run", "task"];
            if layer == "flag" {
                args.extend([
                    "--model",
                    "anthropic/flag-root",
                    "--llm-model",
                    "anthropic/flag-leaf",
                    "--effort",
                    "max",
                    "--base-url",
                    "http://127.0.0.1/flag",
                ]);
            }
            let Some(Command::Run(mut run)) = Cli::parse_from(args).command else {
                panic!("expected run")
            };
            let resolved =
                Resolved::new(&mut run, &config, std::path::Path::new(config::FILE_NAME)).unwrap();
            let provider_config = config
                .anthropic(
                    std::path::Path::new(config::FILE_NAME),
                    run.base_url.as_deref(),
                )
                .unwrap();
            assert_eq!(
                provider_config.base_url,
                if layer == "default" {
                    kyora_providers::anthropic::AnthropicConfig::new("test-only-key").base_url
                } else {
                    format!("http://127.0.0.1/{layer}")
                }
            );
            if layer == "default" {
                assert_eq!(resolved.model.to_string(), defaults::DEFAULT_MODEL);
                assert_eq!(resolved.llm_model.to_string(), defaults::DEFAULT_LLM_MODEL);
                assert_eq!(resolved.effort, RequestOptions::default().effort);
            } else {
                assert_eq!(
                    resolved.model.to_string(),
                    format!("anthropic/{layer}-root")
                );
                assert_eq!(
                    resolved.llm_model.to_string(),
                    format!("anthropic/{layer}-leaf")
                );
                assert_eq!(
                    resolved.effort,
                    Some(match layer.as_str() {
                        "flag" => Effort::Max,
                        "env" => Effort::High,
                        _ => Effort::Low,
                    })
                );
            }
            return;
        }
        for layer in ["default", "config", "env", "flag"] {
            let home = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "tests::config_resolution_precedence"])
                .env(CHILD, layer)
                .env("HOME", home.path())
                .env("KYORA_HOME", home.path())
                .env_remove("KYORA_MODEL")
                .env_remove("KYORA_LLM_MODEL")
                .env_remove("KYORA_EFFORT")
                .env_remove("ANTHROPIC_BASE_URL")
                .env("ANTHROPIC_API_KEY", "test-only-key");
            if layer == "env" || layer == "flag" {
                command
                    .env("KYORA_MODEL", "anthropic/env-root")
                    .env("KYORA_LLM_MODEL", "anthropic/env-leaf")
                    .env("KYORA_EFFORT", "high")
                    .env("ANTHROPIC_BASE_URL", "http://127.0.0.1/env");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "layer {layer}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

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
