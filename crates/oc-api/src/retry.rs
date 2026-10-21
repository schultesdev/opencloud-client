//! Retry logic: exponential backoff with optional jitter.
//!
//! [`RetryConfig`] describes the policy, [`with_retry`] applies it to a
//! fallible async operation whose errors implement the classification via
//! [`ApiError::is_retryable`]. The API clients additionally honor the
//! `Retry-After` hint of 429 responses.

use std::future::Future;
use std::time::Duration;

use crate::error::ApiError;

/// Retry policy for API requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    /// Total number of attempts, including the first one.
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles for every further attempt.
    pub initial_backoff: Duration,
    /// Upper bound for the computed delay.
    pub max_backoff: Duration,
    /// Randomize the delay (factor 0.5–1.5) to avoid thundering herds.
    pub jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            jitter: true,
        }
    }
}

impl RetryConfig {
    /// Deterministic, zero-delay policy for tests.
    pub fn immediate() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            jitter: false,
        }
    }

    /// Delay before the attempt following the given (1-based) attempt
    /// number, with exponential growth capped at `max_backoff`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let base = self
            .initial_backoff
            .saturating_mul(1u32 << shift)
            .min(self.max_backoff);
        if !self.jitter {
            return base;
        }
        let factor = 0.5 + rand::random::<f64>();
        base.mul_f64(factor)
    }
}

/// Runs `op` until it succeeds, fails with a non-retryable
/// [`ApiError`], or the attempts are exhausted; sleeps the computed backoff
/// between attempts.
pub async fn with_retry<T, F, Fut>(config: &RetryConfig, mut op: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    let max_attempts = config.max_attempts.max(1);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= max_attempts || !err.is_retryable() {
                    return Err(err);
                }
                tokio::time::sleep(config.backoff(attempt)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(500),
            jitter: false,
        };
        assert_eq!(config.backoff(1), Duration::from_millis(100));
        assert_eq!(config.backoff(2), Duration::from_millis(200));
        assert_eq!(config.backoff(3), Duration::from_millis(400));
        assert_eq!(config.backoff(4), Duration::from_millis(500));
        assert_eq!(config.backoff(90), Duration::from_millis(500));
    }

    #[test]
    fn jitter_stays_within_half_to_one_and_a_half() {
        let config = RetryConfig {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(10),
            jitter: true,
            ..RetryConfig::default()
        };
        for _ in 0..16 {
            let delay = config.backoff(1);
            assert!(delay >= Duration::from_millis(500));
            assert!(delay <= Duration::from_millis(1500));
        }
    }

    #[tokio::test]
    async fn retries_transient_errors_until_success() {
        let config = RetryConfig::immediate();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let result: Result<usize, ApiError> = with_retry(&config, move || {
            let counter = std::sync::Arc::clone(&counter);
            async move {
                let calls = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if calls < 3 {
                    Err(ApiError::Server { code: 503 })
                } else {
                    Ok(calls)
                }
            }
        })
        .await;
        assert_eq!(result.expect("succeeds on third attempt"), 3);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retryable_errors_short_circuit() {
        let config = RetryConfig::immediate();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let result: Result<(), ApiError> = with_retry(&config, move || {
            let counter = std::sync::Arc::clone(&counter);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ApiError::PreconditionFailed)
            }
        })
        .await;
        assert!(matches!(result, Err(ApiError::PreconditionFailed)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_attempts_and_returns_last_error() {
        let config = RetryConfig::immediate();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let result: Result<(), ApiError> = with_retry(&config, move || {
            let counter = std::sync::Arc::clone(&counter);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ApiError::Server { code: 502 })
            }
        })
        .await;
        assert!(matches!(result, Err(ApiError::Server { code: 502 })));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst) as u32,
            config.max_attempts
        );
    }
}
