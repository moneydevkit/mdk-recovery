//! HTTP-aware retry wrapper for esplora GETs.
//!
//! The wrapper takes a closure that fires a single HTTP request and
//! drives it under a [`RetryPolicy`]. Classification lives inside the
//! loop: connection errors and 429/5xx responses count as transient
//! and trigger exponential backoff; other 4xx responses short-circuit
//! as permanent. Body decoding is the caller's job — once the
//! response is in hand, the retry budget is gone.
//!
//! The status classification is a pure [`classify_status`] function
//! tested independently from the I/O. The loop itself is a thin
//! match-and-sleep over it.
//!
//! The wrapper assumes every error category maps onto
//! [`RecoveryError::Esplora`]. When a future caller wants a different
//! error variant, we can take a constructor closure; until then,
//! parameterising for one hypothetical use is just noise.

use std::future::Future;
use std::time::Duration;

use reqwest::Response;
use tokio::time::sleep;

use crate::error::{RecoveryError, Result, fmt_error_chain};

/// Knobs for the exponential-backoff retry loop. `max_attempts`
/// counts the initial try plus retries; `base_delay` is the wait
/// before the first retry; `max_delay` caps the doubling.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

/// Run `op` under `policy`. The closure fires one HTTP request per
/// attempt; the wrapper classifies the outcome, retries transient
/// failures with exponential backoff, and surfaces permanent
/// failures (or an exhausted budget) as [`RecoveryError::Esplora`].
/// On success the raw `reqwest::Response` is returned so the caller
/// can decode its body.
pub async fn with_retry<F, Fut>(policy: RetryPolicy, op: F) -> Result<Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = reqwest::Result<Response>>,
{
    for attempt in 0..policy.max_attempts {
        match classify(op().await) {
            Outcome::Done(r) => return Ok(r),
            Outcome::Permanent(msg) => return Err(RecoveryError::Esplora(msg)),
            Outcome::Transient(msg) => match backoff_for(attempt, policy) {
                Some(delay) => sleep(delay).await,
                None => return Err(RecoveryError::Esplora(msg)),
            },
        }
    }
    unreachable!("loop exits via early return on every branch")
}

/// What to do with one attempt's outcome.
enum Outcome {
    Done(Response),
    Transient(String),
    Permanent(String),
}

/// How a single HTTP status code is treated. Used as the bridge
/// between the pure classifier and the response-consuming
/// [`classify`] adapter so the rules can be tested without
/// constructing a real `reqwest::Response`.
#[derive(Debug, PartialEq, Eq)]
enum StatusClass {
    Success,
    Transient,
    Permanent,
}

/// Pure: classify an HTTP status code. 429 from nginx's `limit_req`
/// plus the full 5xx range count as transient; everything else
/// outside 2xx is permanent. Splitting the rule out of the loop lets
/// us test the boundaries (200, 299, 300, 404, 429, 500, 599)
/// without an HTTP server.
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
        // Connection-level failures (DNS, TLS, connection reset,
        // timeout) are transient by nature: a fresh attempt has every
        // chance of succeeding once whatever blip caused it has
        // passed.
        Err(e) => Outcome::Transient(fmt_error_chain(&e)),
        Ok(r) => match classify_status(r.status().as_u16()) {
            StatusClass::Success => Outcome::Done(r),
            StatusClass::Transient => Outcome::Transient(format!("status {}", r.status())),
            StatusClass::Permanent => Outcome::Permanent(format!("status {}", r.status())),
        },
    }
}

/// Pure: pick the sleep before retry `attempt + 1`, given a
/// 0-indexed `attempt` that just failed. `None` means the budget is
/// spent and the caller should surface the failure. Delay doubles
/// each attempt and is clamped at `policy.max_delay`.
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

    /// 429 and the full 5xx range are transient. Everything outside
    /// 2xx that isn't a transient is permanent. The boundaries (200,
    /// 299, 300, 500, 599, 600) matter because off-by-one in the
    /// ranges would flip the classification of a real production
    /// response.
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

    /// Backoff doubles each attempt and clamps at `max_delay`. The
    /// final attempt returns `None` so the retry loop knows to give
    /// up rather than sleep forever.
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

    /// Doubling must not overflow the `Duration` multiplication or
    /// the shift used to compute the factor — a pathological
    /// `max_attempts` shouldn't crash the loop, just hit the clamp.
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

    /// `max_attempts == 1` means no retries — the very first attempt
    /// failing must return `None`.
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
