//! HTTP-aware retry wrapper for esplora GETs.
//!
//! Runs a request closure under a [`RetryPolicy`]. Connection errors
//! and 429/5xx count as transient and trigger exponential backoff;
//! other 4xx short-circuit as permanent. Body decoding is the
//! caller's job. Rate-limiting belongs in the closure itself.

use std::future::Future;
use std::time::Duration;

use reqwest::Response;
use tokio::time::sleep;

use crate::error::{RecoveryError, Result, fmt_error_chain};

/// Exponential-backoff knobs. `max_attempts` counts the initial try
/// plus retries; `base_delay` precedes the first retry; `max_delay`
/// caps the doubling.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

/// Run `op` under `policy`. On transient outcomes (429/5xx or local
/// blips) sleep the backoff and retry; on permanent outcomes
/// short-circuit.
pub async fn with_retry<F, Fut>(policy: RetryPolicy, op: F) -> Result<Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = reqwest::Result<Response>>,
{
    for attempt in 0..policy.max_attempts {
        match classify(op().await) {
            Outcome::Done(r) => return Ok(r),
            Outcome::Permanent(msg) => return Err(RecoveryError::Esplora(msg)),
            Outcome::Transient(msg) => {
                let Some(delay) = backoff_for(attempt, policy) else {
                    return Err(RecoveryError::Esplora(msg));
                };
                sleep(delay).await;
            }
        }
    }
    unreachable!("loop exits via early return on every branch")
}

/// One attempt's outcome.
enum Outcome {
    Done(Response),
    Transient(String),
    Permanent(String),
}

/// Bridge between [`classify_status`] and the response-consuming
/// [`classify`] adapter so the rules can be tested without a real
/// `reqwest::Response`.
#[derive(Debug, PartialEq, Eq)]
enum StatusClass {
    Success,
    Transient,
    Permanent,
}

/// 429 and 5xx are transient; everything outside 2xx is permanent.
fn classify_status(status: u16) -> StatusClass {
    if status == 429 || (500..600).contains(&status) {
        StatusClass::Transient
    } else if (200..300).contains(&status) {
        StatusClass::Success
    } else {
        StatusClass::Permanent
    }
}

fn classify(result: reqwest::Result<Response>) -> Outcome {
    match result {
        // DNS/TLS/timeout/reset: a fresh attempt has every chance
        // once the blip passes.
        Err(e) => Outcome::Transient(fmt_error_chain(&e)),
        Ok(r) => match classify_status(r.status().as_u16()) {
            StatusClass::Success => Outcome::Done(r),
            StatusClass::Transient => Outcome::Transient(format!("status {}", r.status())),
            StatusClass::Permanent => Outcome::Permanent(format!("status {}", r.status())),
        },
    }
}

/// Sleep before retry `attempt + 1` (0-indexed). `None` means the
/// budget is spent. Doubles each attempt, clamped at `max_delay`.
fn backoff_for(attempt: u32, policy: RetryPolicy) -> Option<Duration> {
    if attempt + 1 >= policy.max_attempts {
        return None;
    }
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    Some(
        policy
            .base_delay
            .saturating_mul(factor)
            .min(policy.max_delay),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundaries: 200, 299, 300, 429, 500, 599, 600.
    #[test]
    fn classify_status_covers_each_class_and_boundary() {
        assert_eq!(classify_status(200), StatusClass::Success);
        assert_eq!(classify_status(299), StatusClass::Success);
        assert_eq!(classify_status(300), StatusClass::Permanent);
        assert_eq!(classify_status(400), StatusClass::Permanent);
        assert_eq!(classify_status(404), StatusClass::Permanent);
        assert_eq!(classify_status(429), StatusClass::Transient);
        assert_eq!(classify_status(500), StatusClass::Transient);
        assert_eq!(classify_status(503), StatusClass::Transient);
        assert_eq!(classify_status(599), StatusClass::Transient);
        assert_eq!(classify_status(600), StatusClass::Permanent);
    }

    /// Doubles, clamps, then gives up on the final attempt.
    #[test]
    fn backoff_for_doubles_then_clamps_then_gives_up() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(backoff_for(0, policy), Some(Duration::from_millis(500)));
        assert_eq!(backoff_for(1, policy), Some(Duration::from_secs(1)));
        assert_eq!(backoff_for(2, policy), Some(Duration::from_secs(2)));
        assert_eq!(backoff_for(3, policy), Some(Duration::from_secs(4)));
        assert_eq!(backoff_for(4, policy), None);
    }

    /// Large `attempt` must clamp rather than overflow the shift or
    /// the `Duration` multiplication.
    #[test]
    fn backoff_for_clamps_at_max_delay_on_large_attempts() {
        let policy = RetryPolicy {
            max_attempts: 64,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(backoff_for(31, policy), Some(Duration::from_secs(8)));
        assert_eq!(backoff_for(40, policy), Some(Duration::from_secs(8)));
    }

    /// `max_attempts == 1`: no retries.
    #[test]
    fn backoff_for_single_attempt_policy_never_retries() {
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(backoff_for(0, policy), None);
    }
}
