//! Command-line entry point for kyora.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kyora", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a task.
    Run { task: String },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run { .. } => {
            eprintln!("error: not implemented yet");
            ExitCode::from(2)
        }
    }
}
