//! How much traffic is allowed, expressed once and shared by every
//! algorithm.
//!
//! A [`Quota`] is deliberately the *only* configuration type in the
//! crate. Four algorithms that each took their own tuning parameters
//! would make them impossible to compare, and comparing them is the point
//! — the README's table and the cross-algorithm tests in
//! `tests/comparison.rs` both work by handing the same `Quota` to every
//! limiter and recording where they disagree.

use std::time::Duration;

use crate::clock::duration_to_nanos;
use crate::error::QuotaError;

/// A rate: `count` permits per `period`, with a burst allowance.
///
/// # The two numbers mean different things
///
/// `count` per `period` fixes the **long-run** rate. `burst` fixes how
/// far ahead of that rate a caller may get in one instant. They are
/// separable on purpose: "10 requests per second" as a sustained rate is
/// almost always wanted alongside "and a client that has been quiet for a
/// while may send 10 at once", and those are two different limits.
///
/// `burst` defaults to `count`, which is the behaviour most people expect
/// from "10 per second". [`Quota::with_burst`] changes it:
/// `with_burst(1)` is a strict pacer that never allows two permits at the
/// same instant, and a burst larger than `count` allows a bigger opening
/// gulp than the sustained rate would suggest.
///
/// The window-based limiters ([`FixedWindow`](crate::FixedWindow),
/// [`SlidingWindow`](crate::SlidingWindow)) have no separate burst
/// concept — their burst allowance *is* `count`, since a window counter
/// will happily admit the whole window's budget in one instant — so they
/// ignore `burst` entirely. This is stated on those types too, and their
/// tests assert it.
///
/// # Normalisation
///
/// A quota is stored as an **emission interval**: `period / count`,
/// truncated to whole nanoseconds. That is the spacing the token bucket
/// and GCRA actually enforce, and keeping it as an integer keeps every
/// later calculation exact — no floating point, so no drift over a long
/// run and no platform-dependent rounding in a test.
///
/// Truncation makes the real rate very slightly *faster* than nominal
/// when `period` is not divisible by `count`: 3 per second becomes one
/// per 333,333,333 ns, or 3.000000003 per second. The error is bounded by
/// one nanosecond per permit, and [`Quota::emission_interval`] reports
/// exactly what was stored so it is never a surprise.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::Quota;
///
/// let quota = Quota::per_second(4)?.with_burst(10)?;
/// assert_eq!(quota.count(), 4);
/// assert_eq!(quota.burst(), 10);
/// assert_eq!(quota.emission_interval(), Duration::from_millis(250));
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    count: u32,
    period_nanos: u64,
    burst: u32,
    emission_interval_nanos: u64,
}

impl Quota {
    /// `count` permits per `period`, with `burst` defaulting to `count`.
    ///
    /// # Errors
    ///
    /// * [`QuotaError::ZeroCount`] if `count` is zero — a limiter that
    ///   permits nothing is a bug in the caller's configuration, not a
    ///   rate.
    /// * [`QuotaError::ZeroPeriod`] if `period` is zero.
    /// * [`QuotaError::RateTooHigh`] if `period / count` truncates to
    ///   zero nanoseconds, i.e. more than one permit per nanosecond.
    ///   There is no representable spacing for such a rate, and a limiter
    ///   that let everything through would be a silent failure.
    /// * [`QuotaError::BurstTooLarge`] if `burst * (period / count)`
    ///   overflows [`u64`] nanoseconds. Rejecting it here is what lets
    ///   every limiter's hot path use plain `u64` arithmetic without
    ///   overflow checks.
    ///
    /// ```
    /// use std::time::Duration;
    /// use ratelimit::{Quota, QuotaError};
    ///
    /// assert!(Quota::new(30, Duration::from_secs(60)).is_ok());
    /// assert_eq!(Quota::new(0, Duration::from_secs(1)), Err(QuotaError::ZeroCount));
    /// assert_eq!(Quota::new(1, Duration::ZERO), Err(QuotaError::ZeroPeriod));
    /// ```
    pub fn new(count: u32, period: Duration) -> Result<Self, QuotaError> {
        if count == 0 {
            return Err(QuotaError::ZeroCount);
        }
        let period_nanos = duration_to_nanos(period);
        if period_nanos == 0 {
            return Err(QuotaError::ZeroPeriod);
        }
        let emission_interval_nanos = period_nanos / u64::from(count);
        if emission_interval_nanos == 0 {
            return Err(QuotaError::RateTooHigh);
        }
        let quota = Self {
            count,
            period_nanos,
            burst: count,
            emission_interval_nanos,
        };
        quota.check_burst()?;
        Ok(quota)
    }

    /// `count` permits per second.
    ///
    /// # Errors
    ///
    /// As [`Quota::new`].
    pub fn per_second(count: u32) -> Result<Self, QuotaError> {
        Self::new(count, Duration::from_secs(1))
    }

    /// `count` permits per minute.
    ///
    /// # Errors
    ///
    /// As [`Quota::new`].
    pub fn per_minute(count: u32) -> Result<Self, QuotaError> {
        Self::new(count, Duration::from_secs(60))
    }

    /// `count` permits per hour.
    ///
    /// # Errors
    ///
    /// As [`Quota::new`].
    pub fn per_hour(count: u32) -> Result<Self, QuotaError> {
        Self::new(count, Duration::from_secs(3600))
    }

    /// Replaces the burst allowance, leaving the sustained rate alone.
    ///
    /// # Errors
    ///
    /// * [`QuotaError::ZeroBurst`] if `burst` is zero: a limiter that can
    ///   hold no permits at all can never allow one.
    /// * [`QuotaError::BurstTooLarge`] if the resulting bucket depth
    ///   overflows [`u64`] nanoseconds.
    pub fn with_burst(self, burst: u32) -> Result<Self, QuotaError> {
        if burst == 0 {
            return Err(QuotaError::ZeroBurst);
        }
        let quota = Self { burst, ..self };
        quota.check_burst()?;
        Ok(quota)
    }

    /// The permit count this quota was built from.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// The period this quota was built from.
    #[must_use]
    pub const fn period(&self) -> Duration {
        Duration::from_nanos(self.period_nanos)
    }

    /// The burst allowance: the most permits obtainable at a single
    /// instant.
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// The stored spacing between permits, `period / count` truncated to
    /// whole nanoseconds.
    #[must_use]
    pub const fn emission_interval(&self) -> Duration {
        Duration::from_nanos(self.emission_interval_nanos)
    }

    /// The stored spacing between permits, in nanoseconds.
    pub(crate) const fn emission_interval_nanos(&self) -> u64 {
        self.emission_interval_nanos
    }

    /// The period, in nanoseconds. Used by the window limiters, which
    /// treat it as the window length rather than as a rate denominator.
    pub(crate) const fn period_nanos(&self) -> u64 {
        self.period_nanos
    }

    /// The full bucket depth in nanoseconds of credit: `burst *
    /// emission_interval`. Guaranteed by construction not to overflow —
    /// [`Quota::check_burst`] rejects any quota where it would.
    pub(crate) const fn capacity_nanos(&self) -> u64 {
        self.emission_interval_nanos * self.burst as u64
    }

    fn check_burst(&self) -> Result<(), QuotaError> {
        u64::from(self.burst)
            .checked_mul(self.emission_interval_nanos)
            .map(|_| ())
            .ok_or(QuotaError::BurstTooLarge)
    }
}
