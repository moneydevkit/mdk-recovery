//! Deadline-based async throttle for esplora calls.
//!
//! Stock `blockstream/esplora` nginx rate-limits `/api/*` at 5 r/s
//! per IP; the MDK endpoint runs the same. A concurrency cap doesn't
//! fix it because RPS is `concurrency / latency`, not concurrency.
//!
//! The throttle tracks the [`Instant`] the next request may fire at.
//! [`Throttle::acquire`] sleeps until then and advances the deadline
//! by one `interval`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Async throttle. One per scan.
pub struct Throttle {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Throttle {
    /// Fire at most `rate_per_sec` requests per second. The first
    /// acquire fires immediately.
    pub fn new(rate_per_sec: f64) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / rate_per_sec),
            next: Mutex::new(Instant::now()),
        }
    }

    /// No-op throttle. Every `acquire` returns immediately.
    pub fn unlimited() -> Self {
        Self {
            interval: Duration::ZERO,
            next: Mutex::new(Instant::now()),
        }
    }

    /// Sleep until the next slot opens, then claim it. The sleep
    /// runs outside the lock so contenders don't pile up.
    pub async fn acquire(&self) {
        let wait = {
            let mut next = self.next.lock().expect("throttle mutex poisoned");
            let (new_next, wait) = schedule(*next, Instant::now(), self.interval);
            *next = new_next;
            wait
        };
        if !wait.is_zero() {
            sleep(wait).await;
        }
    }
}

/// New deadline and the wait to sleep before firing. An idle
/// throttle (deadline in the past) fires immediately; idle time
/// doesn't bank credit.
fn schedule(next: Instant, now: Instant, interval: Duration) -> (Instant, Duration) {
    let scheduled = next.max(now);
    (
        scheduled + interval,
        scheduled.saturating_duration_since(now),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Idle throttle fires immediately; deadline jumps to `now + interval`.
    #[test]
    fn schedule_fires_immediately_when_deadline_is_past() {
        let now = Instant::now();
        let next = now - Duration::from_secs(1);
        let interval = Duration::from_millis(200);
        let (new_next, wait) = schedule(next, now, interval);
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(new_next, now + interval);
    }

    /// Future deadline forces the caller to wait the remainder.
    #[test]
    fn schedule_waits_remainder_when_deadline_is_future() {
        let now = Instant::now();
        let next = now + Duration::from_millis(150);
        let interval = Duration::from_millis(200);
        let (new_next, wait) = schedule(next, now, interval);
        assert_eq!(wait, Duration::from_millis(150));
        assert_eq!(new_next, next + interval);
    }
}
