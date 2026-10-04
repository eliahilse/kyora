//! Retry decisions for model attempts.

use std::time::Duration;

use crate::ProviderError;

/// Decides whether and when to retry a failed model attempt.
///
/// Delays use the provider's `retry-after` when present, otherwise
/// exponential backoff with full jitter. A 429 without `retry-after` may be a
/// spend cap that keeps failing, so it is retried at most
/// `max_unhinted_429_retries` times. The random input is passed in so
/// decisions are deterministic in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum retries after the first attempt.
    pub max_retries: u32,
    /// Backoff base delay.
    pub base: Duration,
    /// Upper bound for one backoff delay and for an honored `retry-after`.
    pub cap: Duration,
    /// Maximum retries of a 429 that carries no `retry-after`.
    pub max_unhinted_429_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            max_unhinted_429_retries: 1,
        }
    }
}

impl RetryPolicy {
    /// Returns the delay before the next attempt, or `None` to give up.
    ///
    /// `failures` counts failed attempts so far including this one (1 after
    /// the first failure), `unhinted_429s` counts 429 responses without
    /// `retry-after` so far including this one, and `random` is a value in
    /// `[0, 1)` used for jitter.
    pub fn next_delay(
        &self,
        error: &ProviderError,
        failures: u32,
        unhinted_429s: u32,
        random: f64,
    ) -> Option<Duration> {
        if !error.is_retryable() || failures > self.max_retries {
            return None;
        }
        if let Some(delay) = error.retry_after() {
            return Some(delay.min(self.cap));
        }
        if matches!(error, ProviderError::Http { status: 429, .. })
            && unhinted_429s > self.max_unhinted_429_retries
        {
            return None;
        }
        let exponent = failures.saturating_sub(1).min(30);
        let ceiling = self.base.saturating_mul(1u32 << exponent).min(self.cap);
        Some(ceiling.mul_f64(random.clamp(0.0, 1.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, retry_after: Option<u64>) -> ProviderError {
        ProviderError::Http {
            status,
            message: String::new(),
            retry_after: retry_after.map(Duration::from_secs),
        }
    }

    #[test]
    fn gives_up_on_non_retryable_errors_and_after_max_retries() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.next_delay(&http(400, None), 1, 0, 0.5), None);
        assert!(policy.next_delay(&http(500, None), 4, 0, 0.5).is_some());
        assert_eq!(policy.next_delay(&http(500, None), 5, 0, 0.5), None);
    }

    #[test]
    fn honors_retry_after_up_to_the_cap() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.next_delay(&http(429, Some(7)), 1, 0, 0.0),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            policy.next_delay(&http(529, Some(600)), 1, 0, 0.0),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn backoff_is_exponential_with_full_jitter_and_capped() {
        let policy = RetryPolicy::default();
        let error = ProviderError::Transport("reset".into());
        assert_eq!(
            policy
                .next_delay(&error, 1, 0, 0.999_999)
                .unwrap()
                .as_secs(),
            0
        );
        assert_eq!(
            policy.next_delay(&error, 3, 0, 0.5),
            Some(Duration::from_secs(2))
        );
        assert_eq!(policy.next_delay(&error, 1, 0, 0.0), Some(Duration::ZERO));
        let long = RetryPolicy {
            max_retries: 40,
            ..RetryPolicy::default()
        };
        assert_eq!(
            long.next_delay(&error, 40, 0, 1.0),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn unhinted_429_is_retried_at_most_once_by_default() {
        let policy = RetryPolicy::default();
        assert!(policy.next_delay(&http(429, None), 1, 1, 0.5).is_some());
        assert_eq!(policy.next_delay(&http(429, None), 2, 2, 0.5), None);
        assert!(policy.next_delay(&http(429, Some(1)), 2, 2, 0.5).is_some());
    }
}
