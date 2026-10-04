//! Unix shell and atomic file tools. No execution sandbox is provided in M1.
#![cfg(unix)]
pub mod defaults;
mod files;
mod shell;
use anyhow::Result;
pub use files::{EditFile, ReadFile, WriteFile};
use kyora_core::Toolset;
pub use shell::{Shell, ShellConfig, cancel_processes};
use std::sync::Arc;
/// Builds the four M1 tools with configurable shell and file defaults.
pub fn toolset(shell: ShellConfig, files: defaults::FileConfig) -> Result<Toolset> {
    Toolset::new(vec![
        Arc::new(Shell::new(shell)),
        Arc::new(ReadFile::new(files.clone())),
        Arc::new(WriteFile),
        Arc::new(EditFile::default()),
    ])
}
