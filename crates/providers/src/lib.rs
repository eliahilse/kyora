//! Provider interfaces, stream collection, and deterministic in-memory fakes.

use std::{pin::Pin, time::Duration};

use async_trait::async_trait;
use futures::Stream;
use kyora_protocol::{ModelInfo, ModelRequest, StreamEvent};
use tokio_util::sync::CancellationToken;

mod accumulator;
pub use accumulator::{Accumulator, collect};

mod retry;
pub use retry::RetryPolicy;

/// Deterministic fake providers for tests and fixture scripts.
pub mod fake;

/// An error reported by a model provider.
///
/// Variants are chosen so the caller can decide both whether to retry
/// ([`ProviderError::is_retryable`]) and what an attempt costs when it fails
/// ([`ProviderError::charge`]).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// An unsuccessful HTTP response received before any stream event.
    #[error("HTTP {status}: {message}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message.
        message: String,
        /// The provider's optional retry delay.
        retry_after: Option<Duration>,
    },
    /// The request was never sent: DNS, connect or TLS failure before the body was written.
    #[error("not sent: {0}")]
    NotSent(String),
    /// A connection or transport failure after the request was sent.
    #[error("transport: {0}")]
    Transport(String),
    /// An error event inside an otherwise successful stream.
    #[error("stream error {kind}: {message}")]
    Stream {
        /// The provider's error type, for example `overloaded_error`.
        kind: String,
        /// The provider's error message.
        message: String,
    },
    /// No stream event (including keep-alive pings) arrived within the idle timeout.
    #[error("stream idle timeout")]
    IdleTimeout,
    /// A malformed or incomplete provider stream.
    #[error("protocol: {0}")]
    Protocol(String),
    /// The request was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// The request exceeded the model's context limit.
    #[error("context too large: {0}")]
    ContextTooLarge(String),
    /// Another provider failure.
    #[error("{0}")]
    Other(String),
}

/// What a failed attempt is charged, following the settlement policy in docs/design.md 10.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptCharge {
    /// Nothing: the request was not sent, or was rejected before processing.
    Zero,
    /// The attempt's full reservation, because usage is unknown.
    Reserved,
}

impl ProviderError {
    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::NotSent(_) | Self::Transport(_) | Self::IdleTimeout => true,
            Self::Http { status, .. } => {
                matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
            }
            Self::Stream { kind, .. } => matches!(kind.as_str(), "overloaded_error" | "api_error"),
            Self::Protocol(_) | Self::Cancelled | Self::ContextTooLarge(_) | Self::Other(_) => {
                false
            }
        }
    }

    /// What the failed attempt is charged when no usage was reported.
    ///
    /// HTTP 4xx responses and 529 before any stream event are not charged by
    /// policy; other failures after sending are charged conservatively.
    /// `Cancelled` is charged as reserved: callers that cancel before sending
    /// must not dispatch at all, so a cancellation here may follow sending.
    pub fn charge(&self) -> AttemptCharge {
        match self {
            Self::NotSent(_) | Self::ContextTooLarge(_) => AttemptCharge::Zero,
            Self::Http { status, .. } if (400..500).contains(status) || *status == 529 => {
                AttemptCharge::Zero
            }
            _ => AttemptCharge::Reserved,
        }
    }

    /// The provider's requested retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// A sendable stream of provider-neutral events or provider errors.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// A model backend that can stream responses and honor cancellation.
///
/// One call to [`ModelProvider::stream`] is exactly one attempt: providers do
/// not retry internally. Retries, with a fresh budget reservation per
/// attempt, belong to the caller (see [`RetryPolicy`]).
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Returns this provider's name.
    fn name(&self) -> &str;

    /// Begins streaming a response to an owned request.
    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError>;

    /// Returns what the provider knows about a model's limits.
    ///
    /// The default reports nothing known; callers fall back to configuration.
    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError> {
        Ok(ModelInfo {
            id: model.to_owned(),
            ..ModelInfo::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> ProviderError {
        ProviderError::Http {
            status,
            message: String::new(),
            retry_after: None,
        }
    }

    fn stream(kind: &str) -> ProviderError {
        ProviderError::Stream {
            kind: kind.into(),
            message: String::new(),
        }
    }

    #[test]
    fn retryable_errors() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(http(status).is_retryable(), "{status}");
        }
        for status in [200, 400, 401, 403, 404, 413, 501, 505] {
            assert!(!http(status).is_retryable(), "{status}");
        }
        for error in [
            ProviderError::NotSent("dns".into()),
            ProviderError::Transport("reset".into()),
            ProviderError::IdleTimeout,
            stream("overloaded_error"),
            stream("api_error"),
        ] {
            assert!(error.is_retryable(), "{error}");
        }
        for error in [
            ProviderError::Cancelled,
            ProviderError::Protocol("bad".into()),
            ProviderError::ContextTooLarge("large".into()),
            ProviderError::Other("unknown".into()),
            stream("invalid_request_error"),
        ] {
            assert!(!error.is_retryable(), "{error}");
        }
    }

    #[test]
    fn charges_follow_the_settlement_policy() {
        for error in [
            http(400),
            http(429),
            http(499),
            http(529),
            ProviderError::NotSent("connect".into()),
            ProviderError::ContextTooLarge("large".into()),
        ] {
            assert_eq!(error.charge(), AttemptCharge::Zero, "{error}");
        }
        for error in [
            http(500),
            http(503),
            ProviderError::Transport("reset".into()),
            ProviderError::IdleTimeout,
            ProviderError::Cancelled,
            ProviderError::Protocol("bad".into()),
            ProviderError::Other("unknown".into()),
            stream("overloaded_error"),
        ] {
            assert_eq!(error.charge(), AttemptCharge::Reserved, "{error}");
        }
    }

    #[test]
    fn retry_after_is_exposed_only_for_http() {
        let error = ProviderError::Http {
            status: 429,
            message: String::new(),
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(ProviderError::IdleTimeout.retry_after(), None);
    }
}
