//! Credential values resolved from the environment, removed from everything a server
//! connection reports: startup errors, warnings and tool output.
use crate::config::ServerConfig;
use kyora_core::ToolOutput;
use kyora_protocol::ToolResultPart;
use std::{ffi::OsString, sync::Arc};

/// Replaces a credential value in errors and tool output.
pub const REDACTED: &str = "[redacted]";

/// Values of `env_vars`, `bearer_token_env` and `env_headers` for one server.
#[derive(Clone, Default)]
pub(crate) struct Secrets(Arc<Vec<String>>);

impl Secrets {
    pub(crate) fn resolve(config: &ServerConfig, env: &[(OsString, OsString)]) -> Self {
        let mut values: Vec<String> = config
            .env_vars
            .iter()
            .chain(&config.bearer_token_env)
            .chain(config.env_headers.values())
            .filter_map(|variable| env.iter().find(|(name, _)| name == variable.as_str()))
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
            .collect();
        // Longer values first, so a value containing another is removed whole.
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self(Arc::new(values))
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for value in self.0.iter() {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), REDACTED);
            }
        }
        text
    }

    pub(crate) fn output(&self, mut output: ToolOutput) -> ToolOutput {
        for part in &mut output.content {
            let ToolResultPart::Text { text } = part;
            *text = self.redact(text);
        }
        output
    }
}
