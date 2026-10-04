use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::ProviderError;

pub(super) fn redact(message: &str, key: &str) -> String {
    if key.is_empty() {
        message.into()
    } else {
        message.replace(key, "[redacted]")
    }
}

pub(super) fn transport(error: reqwest::Error) -> ProviderError {
    // Do not format reqwest errors: they can contain URLs and credentials.
    if error.is_connect() || error.is_builder() {
        ProviderError::NotSent("connection or request setup failed".into())
    } else if error.is_timeout() {
        ProviderError::Transport("request timeout".into())
    } else {
        ProviderError::Transport("HTTP transport failed".into())
    }
}

pub(super) fn http(status: u16, headers: &HeaderMap, body: &[u8], key: &str) -> ProviderError {
    let retry_after =
        delay(headers, "retry-after-ms", 0.001).or_else(|| delay(headers, "retry-after", 1.0));
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let kind = error
        .and_then(|value| value["type"].as_str())
        .unwrap_or(match status {
            429 => "rate_limit_error",
            529 => "overloaded_error",
            _ => "unknown_error",
        });
    let detail = error
        .and_then(|value| value["message"].as_str())
        .unwrap_or("unsuccessful HTTP response");
    let message = redact(&format!("{kind}: {detail}"), key);
    let lower = detail.to_ascii_lowercase();
    if matches!(status, 400 | 413)
        && (lower.contains("prompt is too long") || lower.contains("prompt too long"))
    {
        ProviderError::ContextTooLarge(message)
    } else {
        ProviderError::Http {
            status,
            message,
            retry_after,
        }
    }
}

fn delay(headers: &HeaderMap, name: &str, factor: f64) -> Option<Duration> {
    let seconds = headers.get(name)?.to_str().ok()?.parse::<f64>().ok()? * factor;
    Duration::try_from_secs_f64(seconds).ok()
}
