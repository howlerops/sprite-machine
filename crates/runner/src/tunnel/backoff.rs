//! Reconnect backoff: exponential from `base`, factor 2, capped, full jitter (tunnel.md §3.8).

use std::time::Duration;

/// A source of jitter: uniform values in `[0, 1)`. Injectable so tests need no randomness.
pub trait Jitter: Send + 'static {
    fn unit(&mut self) -> f64;
}

/// Jitter from the thread-local RNG.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomJitter;

impl Jitter for RandomJitter {
    fn unit(&mut self) -> f64 {
        rand::random::<f64>()
    }
}

/// Exponential backoff with full jitter: the n-th delay (from 0) is uniform in
/// `[0, min(cap, base * 2^n))`.
pub struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
    jitter: Box<dyn Jitter>,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration, jitter: Box<dyn Jitter>) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            jitter,
        }
    }

    /// The upper bound of the next delay.
    pub fn ceiling(&self) -> Duration {
        let factor = 2f64.powi(i32::try_from(self.attempt.min(40)).unwrap_or(40));
        Duration::from_secs_f64((self.base.as_secs_f64() * factor).min(self.cap.as_secs_f64()))
    }

    /// The delay before the next attempt; the following delay's ceiling doubles.
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        let unit = self.jitter.unit().clamp(0.0, 1.0);
        ceiling.mul_f64(unit)
    }

    /// Back to the minimum delay (a connection stayed up, or `server_restart`).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// How many delays were handed out since the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }
}
