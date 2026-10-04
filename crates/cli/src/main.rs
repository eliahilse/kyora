//! Command-line entry point for kyora.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    /// Run a task.
    Run { task: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Some(Command::Run { .. }) => {
            eprintln!("error: not implemented yet");
            ExitCode::from(2)
        }
        command => {
            let demo = matches!(command, Some(Command::Tui { demo: true }));
            match kyora_tui::run(demo).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
