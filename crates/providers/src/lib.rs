//! Provider interfaces, stream collection, and deterministic in-memory fakes.

use std::{pin::Pin, time::Duration};

use async_trait::async_trait;
use futures::Stream;
use kyora_protocol::{ModelRequest, StreamEvent};
use tokio_util::sync::CancellationToken;

mod accumulator;
pub use accumulator::{Accumulator, collect};

/// Deterministic fake providers for tests and fixture scripts.
pub mod fake;

/// An error reported by a model provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// An unsuccessful HTTP response.
    #[error("HTTP {status}: {message}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message.
        message: String,
        /// The provider's optional retry delay.
        retry_after: Option<Duration>,
    },
    /// A connection or transport failure.
    #[error("transport: {0}")]
    Transport(String),
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

impl ProviderError {
    /// Whether retrying may succeed, based on transport failure or HTTP status.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::Http {
                    status: 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529,
                    ..
                }
        )
    }
}

/// A sendable stream of provider-neutral events or provider errors.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// A model backend that can stream responses and honor cancellation.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_errors() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(
                ProviderError::Http {
                    status,
                    message: String::new(),
                    retry_after: None
                }
                .is_retryable()
            );
        }
        for status in [200, 400, 401, 403, 404, 413, 501, 505] {
            assert!(
                !ProviderError::Http {
                    status,
                    message: String::new(),
                    retry_after: None
                }
                .is_retryable()
            );
        }
        assert!(ProviderError::Transport("offline".into()).is_retryable());
        for error in [
            ProviderError::Cancelled,
            ProviderError::Protocol("bad".into()),
            ProviderError::ContextTooLarge("large".into()),
            ProviderError::Other("unknown".into()),
        ] {
            assert!(!error.is_retryable());
        }
    }
}
