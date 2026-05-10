//! Token-bucket rate limiter for esplora calls.
//!
//! `blockstream/esplora`'s stock nginx config rate-limits `/api/*` per
//! client IP at 5 r/s with `burst=10 nodelay`. A naive parallel scan
//! blows past that and earns 429s; a fixed concurrency cap doesn't fix
//! it because RPS is `concurrency / latency`, not concurrency.
//!
//! The bucket is split between a pure half ([`try_take`]) that does
//! the refill arithmetic against a caller-supplied [`Instant`], and a
//! [`RateLimiter`] shell that hangs a `Mutex<BucketState>` plus
//! `tokio::time::sleep` off it. The shell is too small to test;
//! [`try_take`] carries every interesting case.
//!
//! One limiter per scan — the bucket models the public endpoint, not
//! a process-global resource.
//!
//! [`RateLimiter`]: RateLimiter

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Static configuration for a token bucket: capacity in tokens and
/// refill rate in tokens per second.
#[derive(Debug, Clone, Copy)]
struct BucketConfig {
    capacity: f64,
    refill_per_sec: f64,
}

/// Mutable bucket state: current token count and the `Instant` the
/// refill arithmetic was last applied at.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// Async token-bucket rate limiter. Hold one per scan; share by
/// reference across the per-script fetch tasks.
pub struct RateLimiter {
    cfg: BucketConfig,
    state: Mutex<BucketState>,
}

impl RateLimiter {
    /// Build a limiter that refills at `rate_per_sec` tokens/s up to
    /// a cap of `burst`. The bucket starts full so the first `burst`
    /// requests fire immediately, mirroring nginx's `nodelay` shape.
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            cfg: BucketConfig {
                capacity: burst,
                refill_per_sec: rate_per_sec,
            },
            state: Mutex::new(BucketState {
                tokens: burst,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Block until a token is available, then consume it. Multiple
    /// concurrent acquires serialize on the mutex briefly; the wait
    /// itself happens outside the lock so contenders don't pile up.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut s = self.state.lock().expect("rate limiter mutex poisoned");
                match try_take(*s, Instant::now(), self.cfg) {
                    Ok(next) => {
                        *s = next;
                        return;
                    }
                    Err((advanced, wait)) => {
                        *s = advanced;
                        wait
                    }
                }
            };
            sleep(wait).await;
        }
    }
}

/// Pure half of the bucket: accrue tokens for the elapsed time since
/// `state.last_refill`, clamp to `cfg.capacity`, then either consume
/// one token (`Ok`) or report the duration the caller must wait
/// before a token will be available (`Err`). In the `Err` branch the
/// bucket is still returned with its `last_refill` advanced so the
/// caller can store it without re-doing the work.
fn try_take(
    state: BucketState,
    now: Instant,
    cfg: BucketConfig,
) -> Result<BucketState, (BucketState, Duration)> {
    let elapsed = now
        .saturating_duration_since(state.last_refill)
        .as_secs_f64();
    let refilled = (state.tokens + elapsed * cfg.refill_per_sec).min(cfg.capacity);
    let advanced = BucketState {
        tokens: refilled,
        last_refill: now,
    };
    if refilled >= 1.0 {
        Ok(BucketState {
            tokens: refilled - 1.0,
            ..advanced
        })
    } else {
        let wait = Duration::from_secs_f64((1.0 - refilled) / cfg.refill_per_sec);
        Err((advanced, wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(rate: f64, burst: f64) -> BucketConfig {
        BucketConfig {
            capacity: burst,
            refill_per_sec: rate,
        }
    }

    /// A bucket with at least one token returns it without advancing
    /// time-driven accrual; the count drops by exactly one.
    #[test]
    fn try_take_consumes_when_token_available() {
        let now = Instant::now();
        let state = BucketState {
            tokens: 3.0,
            last_refill: now,
        };
        let next = try_take(state, now, cfg(4.0, 8.0)).expect("token available");
        assert!((next.tokens - 2.0).abs() < 1e-9);
        assert_eq!(next.last_refill, now);
    }

    /// 0.5 s at 4 r/s accrues 2 tokens, leaving 1 after consumption.
    /// Pins the elapsed-time math against floating-point drift.
    #[test]
    fn try_take_refills_then_consumes() {
        let t0 = Instant::now();
        let state = BucketState {
            tokens: 0.0,
            last_refill: t0,
        };
        let now = t0 + Duration::from_millis(500);
        let next = try_take(state, now, cfg(4.0, 8.0)).expect("two tokens accrued");
        assert!((next.tokens - 1.0).abs() < 1e-9);
        assert_eq!(next.last_refill, now);
    }

    /// A long idle period must not accrue more than `capacity` —
    /// otherwise the operator could drain the public endpoint with
    /// one large burst the moment they reconnect.
    #[test]
    fn try_take_clamps_at_capacity() {
        let t0 = Instant::now();
        let state = BucketState {
            tokens: 1.0,
            last_refill: t0,
        };
        let now = t0 + Duration::from_secs(60);
        let next = try_take(state, now, cfg(100.0, 3.0)).expect("plenty of tokens");
        assert!((next.tokens - 2.0).abs() < 1e-9);
    }

    /// An empty bucket reports the wait until the next token. At
    /// 4 r/s a single token costs 250 ms.
    #[test]
    fn try_take_returns_wait_when_empty() {
        let t0 = Instant::now();
        let state = BucketState {
            tokens: 0.0,
            last_refill: t0,
        };
        let (advanced, wait) = try_take(state, t0, cfg(4.0, 8.0)).expect_err("empty");
        assert!((advanced.tokens - 0.0).abs() < 1e-9);
        assert_eq!(advanced.last_refill, t0);
        assert!((wait.as_secs_f64() - 0.25).abs() < 1e-6);
    }

    /// Fractional accrual must carry across calls; otherwise sub-
    /// token elapses get rounded to zero and the limiter undershoots
    /// its rate forever.
    #[test]
    fn try_take_accumulates_fractional_tokens() {
        let t0 = Instant::now();
        let state = BucketState {
            tokens: 0.0,
            last_refill: t0,
        };
        // 100 ms at 4 r/s = 0.4 tokens.
        let t1 = t0 + Duration::from_millis(100);
        let (after_first, _) = try_take(state, t1, cfg(4.0, 8.0)).expect_err("still empty");
        assert!((after_first.tokens - 0.4).abs() < 1e-9);
        // Another 200 ms = 0.8 more tokens, total 1.2, then -1 = 0.2.
        let t2 = t1 + Duration::from_millis(200);
        let after_second = try_take(after_first, t2, cfg(4.0, 8.0)).expect("token available");
        assert!((after_second.tokens - 0.2).abs() < 1e-9);
    }
}
