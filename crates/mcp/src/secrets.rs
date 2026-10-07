//! Credential values resolved from the environment, removed from everything a server
//! connection reports: startup errors, warnings and tool output.
use crate::{config::ServerConfig, defaults};
use kyora_core::ToolOutput;
use kyora_protocol::ToolResultPart;
use std::{ffi::OsString, sync::Arc};

/// Replaces a credential value in errors and tool output.
pub const REDACTED: &str = "[redacted]";

/// Values of `env_vars`, `bearer_token_env` and `env_headers` for one server.
#[derive(Clone, Default)]
pub(crate) struct Secrets(Arc<Patterns>);

#[derive(Default)]
struct Patterns {
    /// Longest first, so the longest value matching at a position wins.
    values: Vec<String>,
    /// Whether some value starts with this byte; most positions are skipped on it.
    first: Vec<bool>,
    /// Variables whose values are too short to redact without shredding output.
    short: Vec<String>,
}

impl Secrets {
    pub(crate) fn resolve(config: &ServerConfig, env: &[(OsString, OsString)]) -> Self {
        let mut values = Vec::new();
        let mut short = Vec::new();
        for variable in config
            .env_vars
            .iter()
            .chain(&config.bearer_token_env)
            .chain(config.env_headers.values())
        {
            let Some((_, value)) = env.iter().find(|(name, _)| name == variable.as_str()) else {
                continue;
            };
            let value = value.to_string_lossy().into_owned();
            if value.chars().count() < defaults::MIN_SECRET_CHARS {
                if !value.is_empty() && !short.contains(variable) {
                    short.push(variable.clone());
                }
                continue;
            }
            values.push(value);
        }
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        let mut first = vec![false; 256];
        for value in &values {
            first[usize::from(value.as_bytes()[0])] = true;
        }
        Self(Arc::new(Patterns {
            values,
            first,
            short,
        }))
    }

    /// Notes for values that are not redacted because they are too short.
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.0
            .short
            .iter()
            .map(|variable| {
                format!(
                    "the value of {variable} has fewer than {} characters, too few to redact; it can appear in tool output",
                    defaults::MIN_SECRET_CHARS
                )
            })
            .collect()
    }

    /// Replaces every value in one pass over `text`. Placeholders are never scanned
    /// again, so the output grows by at most the placeholder per matched value.
    pub(crate) fn redact(&self, text: &str) -> String {
        let patterns = &self.0;
        if patterns.values.is_empty() {
            return text.to_owned();
        }
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let (mut copied, mut at) = (0, 0);
        while at < bytes.len() {
            if patterns.first[usize::from(bytes[at])]
                && let Some(value) = patterns
                    .values
                    .iter()
                    .find(|value| bytes[at..].starts_with(value.as_bytes()))
            {
                // Values are valid UTF-8, so a match starts and ends on char boundaries.
                out.push_str(&text[copied..at]);
                out.push_str(REDACTED);
                at += value.len();
                copied = at;
            } else {
                at += 1;
            }
        }
        out.push_str(&text[copied..]);
        out
    }

    pub(crate) fn output(&self, mut output: ToolOutput) -> ToolOutput {
        for part in &mut output.content {
            let ToolResultPart::Text { text } = part;
            *text = self.redact(text);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> Secrets {
        let env: Vec<(OsString, OsString)> = values
            .iter()
            .enumerate()
            .map(|(i, value)| (format!("V{i}").into(), (*value).into()))
            .collect();
        let config = ServerConfig {
            command: Some("server".into()),
            env_vars: (0..values.len()).map(|i| format!("V{i}")).collect(),
            ..ServerConfig::default()
        };
        Secrets::resolve(&config, &env)
    }

    #[test]
    fn placeholders_are_never_redacted_again() {
        // Each value occurs inside the placeholder or inside another value.
        let secrets = secrets(&["aaaaaa", "redact", "[redacted]", "aaaaaaaa"]);
        assert_eq!(secrets.redact("x aaaaaa y"), "x [redacted] y");
        assert_eq!(secrets.redact("aaaaaaaa"), REDACTED);
        let text = "aaaaaa redact ".repeat(10_000);
        let redacted = secrets.redact(&text);
        assert_eq!(redacted, "[redacted] [redacted] ".repeat(10_000));
        assert!(redacted.len() <= text.len() * REDACTED.len() / defaults::MIN_SECRET_CHARS);
    }

    #[test]
    fn short_values_are_not_redacted_and_are_reported() {
        let secrets = secrets(&["a", "c", "abcde", "longer-value"]);
        assert_eq!(
            secrets.redact("a c abcde longer-value"),
            "a c abcde [redacted]"
        );
        let warnings = secrets.warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("V0") && !warnings.concat().contains("abcde"));
    }

    #[test]
    fn multibyte_text_and_values_stay_valid() {
        let secrets = secrets(&["päßwörd", "ключ-значение"]);
        assert_eq!(
            secrets.redact("é päßwörd ключ-значение ü"),
            "é [redacted] [redacted] ü"
        );
    }
}
