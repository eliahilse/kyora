//! Credential values resolved from the environment, removed from everything a server
//! connection reports: startup errors, warnings, tool specs and tool output.
//!
//! Redaction is best effort by contract. A configured server is trusted with the
//! credentials forwarded to it, so this only keeps common echoes out of traces and
//! model context. Covered: exact text (with overlapping matches merged), JSON string
//! escapes including `\uXXXX` for non-ASCII characters, decoded JSON strings, keys
//! and numbers, and model-visible tool names. Not covered: other encodings such as
//! base64 or percent-encoding inside text. docs/mcp.md states the same scope.
use crate::{config::ServerConfig, defaults};
use kyora_core::ToolOutput;
use kyora_protocol::ToolResultPart;
use serde_json::Value;
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
    /// Collects the values to redact. A value longer than
    /// [`defaults::MAX_SECRET_BYTES`] is refused: it could not fit in a capped error
    /// body or stderr tail, so it could never be recognized whole.
    pub(crate) fn resolve(
        config: &ServerConfig,
        env: &[(OsString, OsString)],
    ) -> anyhow::Result<Self> {
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
            if value.len() > defaults::MAX_SECRET_BYTES {
                anyhow::bail!(
                    "the value of {variable} is longer than {} bytes, too long to redact reliably",
                    defaults::MAX_SECRET_BYTES
                );
            }
            if value.chars().count() < defaults::MIN_SECRET_CHARS {
                if !value.is_empty() && !short.contains(variable) {
                    short.push(variable.clone());
                }
                continue;
            }
            // Text that embeds JSON carries the escaped form, as in "pa\"ss".
            let escaped = serde_json::to_string(&value).expect("strings serialize");
            let escaped = &escaped[1..escaped.len() - 1];
            if escaped != value {
                values.push(escaped.to_owned());
            }
            // ASCII-only encoders, such as Python's json by default, write every other
            // character as \uXXXX, in either case.
            if !value.is_ascii() {
                values.push(ascii_escaped(escaped, false));
                values.push(ascii_escaped(escaped, true));
            }
            values.push(value);
        }
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        let mut first = vec![false; 256];
        for value in &values {
            first[usize::from(value.as_bytes()[0])] = true;
        }
        Ok(Self(Arc::new(Patterns {
            values,
            first,
            short,
        })))
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

    /// Length in bytes of the longest value.
    pub(crate) fn longest(&self) -> usize {
        self.0.values.first().map_or(0, String::len)
    }

    /// Replaces every value in one pass over `text`. Placeholders are never scanned
    /// again, so the output grows by at most the placeholder per matched value.
    pub(crate) fn redact(&self, text: &str) -> String {
        self.redact_from(text, 0)
    }

    /// Like [`Self::redact`], but keeps only what follows byte `from`. Values are
    /// matched in the whole text, so one that straddles `from` is still replaced
    /// instead of leaving its tail behind.
    pub(crate) fn redact_from(&self, text: &str, from: usize) -> String {
        let mut from = from.min(text.len());
        while !text.is_char_boundary(from) {
            from += 1;
        }
        let patterns = &self.0;
        if patterns.values.is_empty() {
            return text[from..].to_owned();
        }
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len() - from);
        let mut copied = from;
        // Spans of matched values. Every position is tried, also inside a span, so
        // overlapping values, as in "abcdef" and "defghi" within "abcdefghi", merge
        // into one span instead of the second being left half visible.
        let mut span: Option<(usize, usize)> = None;
        let emit = |(start, end): (usize, usize), out: &mut String, copied: &mut usize| {
            // Values are valid UTF-8, so spans start and end on char boundaries.
            if end > *copied {
                out.push_str(&text[*copied..start.max(*copied)]);
                out.push_str(REDACTED);
                *copied = end;
            }
        };
        for at in 0..bytes.len() {
            if !patterns.first[usize::from(bytes[at])] {
                continue;
            }
            let Some(value) = patterns
                .values
                .iter()
                .find(|value| bytes[at..].starts_with(value.as_bytes()))
            else {
                continue;
            };
            let end = at + value.len();
            span = match span {
                // Overlapping or adjacent: one placeholder covers both.
                Some((start, last)) if at <= last => Some((start, last.max(end))),
                Some(previous) => {
                    emit(previous, &mut out, &mut copied);
                    Some((at, end))
                }
                None => Some((at, end)),
            };
        }
        if let Some(last) = span {
            emit(last, &mut out, &mut copied);
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Whether `text` contains a value.
    pub(crate) fn found_in(&self, text: &str) -> bool {
        self.redact(text) != text
    }

    /// Redacts every string in `value`, object keys included. A number whose decimal
    /// form contains a value becomes the placeholder string.
    pub(crate) fn redact_json(&self, value: &mut Value) {
        if self.0.values.is_empty() {
            return;
        }
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Number(number) => {
                let text = number.to_string();
                if text.len() >= defaults::MIN_SECRET_CHARS && self.found_in(&text) {
                    *value = Value::String(REDACTED.to_owned());
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_json(item)),
            Value::Object(map) => {
                *map = std::mem::take(map)
                    .into_iter()
                    .map(|(key, mut value)| {
                        self.redact_json(&mut value);
                        (self.redact(&key), value)
                    })
                    .collect();
            }
            _ => {}
        }
    }

    pub(crate) fn output(&self, mut output: ToolOutput) -> ToolOutput {
        for part in &mut output.content {
            let ToolResultPart::Text { text } = part;
            *text = self.redact(text);
        }
        output
    }
}

/// `escaped` with every non-ASCII character as JSON `\uXXXX` UTF-16 units.
fn ascii_escaped(escaped: &str, upper: bool) -> String {
    let mut out = String::with_capacity(escaped.len() * 2);
    for c in escaped.chars() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        for unit in c.encode_utf16(&mut [0; 2]) {
            out.push_str(&if upper {
                format!("\\u{unit:04X}")
            } else {
                format!("\\u{unit:04x}")
            });
        }
    }
    out
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
        Secrets::resolve(&config, &env).unwrap()
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
    fn json_escaped_values_are_redacted_in_text() {
        let secrets = secrets(&["pa\"ss\\word"]);
        let json = serde_json::to_string(&serde_json::json!({"password": "pa\"ss\\word"}));
        assert_eq!(
            secrets.redact(&json.unwrap()),
            r#"{"password":"[redacted]"}"#
        );
        assert_eq!(secrets.redact("raw pa\"ss\\word"), "raw [redacted]");
        let mut value = serde_json::json!({"nested": ["pa\"ss\\word"]});
        secrets.redact_json(&mut value);
        assert_eq!(value, serde_json::json!({"nested": ["[redacted]"]}));
    }

    #[test]
    fn a_value_straddling_the_cut_is_replaced_whole() {
        let secrets = secrets(&["0123456789"]);
        let text = "xx0123456789yy";
        assert_eq!(secrets.redact_from(text, 0), "xx[redacted]yy");
        assert_eq!(secrets.redact_from(text, 6), "[redacted]yy");
        assert_eq!(secrets.redact_from(text, 11), "[redacted]yy");
        // A value that ends at the cut has nothing left to replace.
        assert_eq!(secrets.redact_from(text, 12), "yy");
        assert_eq!(secrets.redact_from(text, 13), "y");
        assert_eq!(secrets.redact_from("ü0123456789", 1), "[redacted]");
    }

    #[test]
    fn overlapping_and_adjacent_values_are_covered_whole() {
        let secrets = secrets(&["abcdef", "defghijklmnop"]);
        assert_eq!(secrets.redact("abcdefghijklmnop"), "[redacted]");
        assert_eq!(secrets.redact("x abcdefghijklmnop y"), "x [redacted] y");
        assert_eq!(secrets.redact_from("abcdefghijklmnop", 6), "[redacted]");
        assert_eq!(secrets.redact("abcdefabcdef!"), "[redacted]!");
    }

    #[test]
    fn numbers_that_carry_a_value_are_replaced() {
        let secrets = secrets(&["12345678"]);
        let mut value = serde_json::json!({"pin": 12345678, "longer": 9912345678u64, "small": 42});
        secrets.redact_json(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"pin": "[redacted]", "longer": "[redacted]", "small": 42})
        );
    }

    #[test]
    fn unicode_escaped_values_are_redacted_in_text() {
        let secrets = secrets(&["pässwörd", "key-\u{1f600}-value"]);
        for text in [
            r#"{"p":"p\u00e4ssw\u00f6rd"}"#,
            r#"{"p":"p\u00E4ssw\u00F6rd"}"#,
            r#"{"p":"key-\ud83d\ude00-value"}"#,
        ] {
            assert_eq!(secrets.redact(text), r#"{"p":"[redacted]"}"#, "{text}");
        }
    }

    #[test]
    fn values_too_long_to_redact_are_refused() {
        let config = ServerConfig {
            command: Some("server".into()),
            env_vars: vec!["LONG".into()],
            ..ServerConfig::default()
        };
        let fits = vec![("LONG".into(), "x".repeat(defaults::MAX_SECRET_BYTES).into())];
        assert!(Secrets::resolve(&config, &fits).is_ok());
        let long = vec![(
            "LONG".into(),
            "x".repeat(defaults::MAX_SECRET_BYTES + 1).into(),
        )];
        let error = Secrets::resolve(&config, &long).err().unwrap().to_string();
        assert!(error.contains("LONG") && error.contains("4096"), "{error}");
        assert!(!error.contains("xxxx"), "{error}");
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
