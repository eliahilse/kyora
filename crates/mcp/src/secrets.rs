//! Credential values resolved from the environment, removed from everything a server
//! connection reports: startup errors, warnings, tool specs and tool output.
//!
//! Redaction is best effort by contract. A configured server is trusted with the
//! credentials forwarded to it, so this only keeps common echoes out of traces and
//! model context. Covered: exact text (with overlapping matches merged), JSON string
//! escapes in any spelling (text is also searched with its escapes decoded), decoded
//! JSON strings, keys and numbers, and model-visible tool names. Not covered: other
//! encodings such as base64 or percent-encoding inside text, and values shorter than
//! 6 characters (characters, not bytes: "密钥值" has 9 bytes but is not redacted).
//! docs/mcp.md states the same scope.
use crate::{config::ServerConfig, defaults};
use aho_corasick::AhoCorasick;
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
    /// Every value, searched in linear passes with overlapping matches reported;
    /// None when nothing is to be redacted.
    automaton: Option<AhoCorasick>,
    /// Length in bytes of the longest value.
    longest: usize,
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
            values.push(value);
        }
        values.sort();
        values.dedup();
        let longest = values.iter().map(String::len).max().unwrap_or(0);
        let automaton = match values.is_empty() {
            true => None,
            false => Some(AhoCorasick::new(&values)?),
        };
        Ok(Self(Arc::new(Patterns {
            automaton,
            longest,
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

    /// Bytes of text a single match can span: the longest value, or six times that
    /// when every character of it is written as a `\uXXXX` escape.
    pub(crate) fn reach(&self) -> usize {
        self.0.longest * ESCAPE_GROWTH
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
        let Some(automaton) = &self.0.automaton else {
            return text[from..].to_owned();
        };
        let reach = self.reach();
        let mut out = String::with_capacity(text.len() - from);
        let mut copied = from;
        let emit = |(start, end): (usize, usize), out: &mut String, copied: &mut usize| {
            // Values are valid UTF-8 and escapes map back whole, so spans start and
            // end on char boundaries.
            if end > *copied {
                out.push_str(&text[*copied..start.max(*copied)]);
                out.push_str(REDACTED);
                *copied = end;
            }
        };
        // Matches in the text itself, and in the text with its JSON escapes decoded,
        // mapped back to the escapes they came from. Overlapping search reports every
        // match, also inside another, in order of their ends; both streams stay in
        // that order and are merged, all in time linear in the text plus the matches.
        let decoded = Decoded::new(text);
        let mut raw = automaton
            .find_overlapping_iter(text)
            .map(|found| (found.start(), found.end()))
            .peekable();
        let mut escaped = decoded
            .iter()
            .flat_map(|view| {
                automaton
                    .find_overlapping_iter(&view.text)
                    .map(|found| view.original(found.start(), found.end()))
            })
            .peekable();
        // Overlapping and adjacent matches, as "abcdef" and "defghi" within
        // "abcdefghi", merge into one span instead of the second being left half
        // visible.
        let mut pending: Vec<(usize, usize)> = Vec::new();
        loop {
            let next = match (raw.peek(), escaped.peek()) {
                (Some(a), Some(b)) if a.1 <= b.1 => raw.next(),
                (_, Some(_)) => escaped.next(),
                (Some(_), None) => raw.next(),
                (None, None) => break,
            };
            let (mut start, end) = next.expect("peeked match");
            while let Some(&(earlier, last)) = pending.last()
                && last >= start
            {
                start = start.min(earlier);
                pending.pop();
            }
            pending.push((start, end));
            // A later match ends at or after `end`, so it starts at or after
            // `end - reach`; spans ending before that are final.
            let settled = pending
                .iter()
                .take_while(|(_, last)| *last < end.saturating_sub(reach))
                .count();
            for span in pending.drain(..settled) {
                emit(span, &mut out, &mut copied);
            }
        }
        for span in pending {
            emit(span, &mut out, &mut copied);
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Whether `text` contains a value.
    pub(crate) fn found_in(&self, text: &str) -> bool {
        self.0.automaton.as_ref().is_some_and(|automaton| {
            automaton.is_match(text)
                || Decoded::new(text).is_some_and(|view| automaton.is_match(&view.text))
        })
    }

    /// Redacts every string in `value`, object keys included. A number whose decimal
    /// form contains a value becomes the placeholder string.
    pub(crate) fn redact_json(&self, value: &mut Value) {
        if self.0.automaton.is_none() {
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

/// Most text bytes per decoded byte: an ASCII character written as `\uXXXX`.
const ESCAPE_GROWTH: usize = 6;

/// `text` with its JSON string escapes decoded, `\uXXXX` in any case and surrogate
/// pairs included, so every spelling of a value meets the same automaton. Invalid
/// or partial escapes stay as they are.
struct Decoded {
    text: String,
    /// For every decoded byte, and one past the end, the text offset of the
    /// character or escape it came from.
    origin: Vec<u32>,
}

impl Decoded {
    /// None when there is nothing to decode, or the text is too large to map.
    fn new(text: &str) -> Option<Self> {
        if !text.contains('\\') || u32::try_from(text.len()).is_err() {
            return None;
        }
        let bytes = text.as_bytes();
        let mut decoded = String::with_capacity(text.len());
        let mut origin = Vec::with_capacity(text.len() + 1);
        let mut at = 0;
        while at < text.len() {
            let (c, used, escape) = match unescape(&bytes[at..]) {
                Some((c, used)) => (c, used, true),
                // Not a valid escape, a stray backslash included: copy it as it is.
                None => {
                    let c = text[at..].chars().next().expect("char boundary");
                    (c, c.len_utf8(), false)
                }
            };
            let start = decoded.len();
            decoded.push(c);
            // Bytes of an escape all map to its start; copied bytes to themselves.
            for offset in 0..decoded.len() - start {
                let from = if escape { at } else { at + offset };
                origin.push(from as u32);
            }
            at += used;
        }
        origin.push(text.len() as u32);
        Some(Self {
            text: decoded,
            origin,
        })
    }

    /// The text range a decoded match came from. Matches start and end on decoded
    /// char boundaries, so the range covers whole escapes.
    fn original(&self, start: usize, end: usize) -> (usize, usize) {
        (self.origin[start] as usize, self.origin[end] as usize)
    }
}

/// The character an escape at the start of `bytes` stands for, and its length.
fn unescape(bytes: &[u8]) -> Option<(char, usize)> {
    if bytes.first() != Some(&b'\\') {
        return None;
    }
    let simple = match bytes.get(1)? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => return unicode(bytes),
        _ => return None,
    };
    Some((simple, 2))
}

/// A `\uXXXX` escape, or a surrogate pair of two.
fn unicode(bytes: &[u8]) -> Option<(char, usize)> {
    let unit = hex4(bytes.get(2..6)?)?;
    match unit {
        0xD800..=0xDBFF => {
            if bytes.get(6..8)? != b"\\u" {
                return None;
            }
            let low = hex4(bytes.get(8..12)?)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return None;
            }
            let c = char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00))?;
            Some((c, 12))
        }
        0xDC00..=0xDFFF => None,
        _ => Some((char::from_u32(unit)?, 6)),
    }
}

fn hex4(digits: &[u8]) -> Option<u32> {
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
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
    fn every_spelling_of_an_escaped_value_is_redacted() {
        let secrets = secrets(&["pässwörd", "pa/ssword", "password"]);
        for text in [
            // Mixed case, partly escaped, an escaped solidus, escaped ASCII letters.
            r#"{"p":"p\u00E4ssw\u00f6rd"}"#,
            r#"{"p":"p\u00e4sswörd"}"#,
            r#"{"p":"pa\/ssword"}"#,
            r#"{"p":"\u0070\u0061ssword"}"#,
        ] {
            assert_eq!(secrets.redact(text), r#"{"p":"[redacted]"}"#, "{text}");
        }
        // The span maps back to the escapes around it, multibyte text included.
        assert_eq!(secrets.redact(r"é \u0070assword ü"), "é [redacted] ü");
        assert!(secrets.found_in(r"\u0070assword"));
    }

    #[test]
    fn invalid_and_partial_escapes_are_left_alone() {
        let secrets = secrets(&["password"]);
        for text in [
            "\\",
            "trailing \\",
            "\\u00",
            "\\uZZZZ password",
            "\\ud83d lone high surrogate",
            "\\udc00 lone low surrogate",
            "\\ud83d\\u0041 unpaired",
            "\\q unknown escape",
            "\\u+123 sign",
            "ü\\",
        ] {
            let expected = text.replace("password", REDACTED);
            assert_eq!(secrets.redact(text), expected, "{text}");
        }
        // An escaped backslash ends the escape, so "\\u0070" is not a "p".
        assert_eq!(secrets.redact("\\\\u0070assword"), "\\\\u0070assword");
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
    fn a_long_repetitive_value_is_redacted_in_linear_time() {
        // Every position of the text starts a match. Trying each value at each
        // position took on the order of 16 MiB times 4 KiB steps; the automaton
        // takes one pass, so even an unoptimized build finishes well within the bound.
        let value = "a".repeat(defaults::MAX_SECRET_BYTES);
        let secrets = secrets(&[&value]);
        let text = "a".repeat(16 * 1024 * 1024);
        let started = std::time::Instant::now();
        assert_eq!(secrets.redact(&text), REDACTED);
        assert_eq!(secrets.redact_from(&format!("x{text}y"), 1), "[redacted]y");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
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
