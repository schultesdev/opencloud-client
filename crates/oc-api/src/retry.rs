//! Retry logic: exponential backoff with optional jitter.
//!
//! [`RetryConfig`] describes the policy; the clients' shared HTTP core
//! applies it per request, classifying failures via
//! [`ApiError::is_retryable`](crate::ApiError::is_retryable) and honoring
//! `Retry-After` hints.

use std::time::Duration;

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
}
