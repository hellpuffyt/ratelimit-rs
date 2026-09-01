//! What a limiter answers, and the trait every limiter answers it
//! through.

use std::time::Duration;

use crate::quota::Quota;

/// The outcome of asking a limiter for permits.
///
/// There are three outcomes, not two, because "no" has two genuinely
/// different meanings and collapsing them loses information the caller
/// needs:
///
/// * [`Decision::Allow`] — take the permits.
/// * [`Decision::Deny`] — not now; the same request would succeed after
///   `retry_after`. This is a *transient* refusal and retrying is the
///   right response.
/// * [`Decision::Exceeds`] — never; the request asks for more permits
///   than the limiter can ever hold at once. Retrying is futile, and a
///   client that retries forever on this has a bug that a two-variant
///   answer would have hidden. Asking a 10-permit bucket for 11 permits
///   is not a rate-limiting event, it is a request that does not fit.
///
/// Wire this straight onto HTTP: `Allow` carries the value for
/// `X-RateLimit-Remaining`, `Deny` carries the value for `Retry-After`,
/// and `Exceeds` is a 400, not a 429.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, ManualClock, Quota, RateLimiter, TokenBucket};
///
/// let clock = ManualClock::new();
/// let mut bucket = TokenBucket::with_clock(Quota::per_second(2)?, &clock);
///
/// assert_eq!(bucket.try_acquire(), Decision::Allow { remaining: 1 });
/// assert_eq!(bucket.try_acquire(), Decision::Allow { remaining: 0 });
/// assert_eq!(
///     bucket.try_acquire(),
///     Decision::Deny { retry_after: Duration::from_millis(500) }
/// );
/// assert_eq!(bucket.try_acquire_n(3), Decision::Exceeds { capacity: 2 });
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The permits were granted.
    Allow {
        /// How many further permits could be taken at this same instant.
        remaining: u32,
    },
    /// The permits were not granted, but would be after waiting.
    Deny {
        /// How long until the same request would succeed, assuming no
        /// other traffic arrives in the meantime.
        ///
        /// This is exact for every limiter in this crate: it is computed
        /// from the limiter's own state, not rounded up to a whole
        /// second or guessed from the quota. Other traffic arriving
        /// first can of course still take the permits.
        retry_after: Duration,
    },
    /// The request asked for more permits than the limiter can ever hold,
    /// so no amount of waiting would help.
    Exceeds {
        /// The most permits this limiter can grant at one instant.
        capacity: u32,
    },
}

impl Decision {
    /// Whether the permits were granted.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    /// Whether the permits were refused, for either reason.
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        !self.is_allowed()
    }

    /// How long to wait before retrying, or [`None`] when retrying would
    /// not help — which covers both [`Decision::Allow`] (nothing to wait
    /// for) and [`Decision::Exceeds`] (waiting changes nothing).
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Deny { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    /// Permits still available at the instant of the decision, or
    /// [`None`] if the request was refused.
    #[must_use]
    pub const fn remaining(&self) -> Option<u32> {
        match self {
            Self::Allow { remaining } => Some(*remaining),
            _ => None,
        }
    }
}

/// The behaviour shared by every algorithm in this crate.
///
/// The methods take `&mut self` because a limiter *is* mutable state:
/// even [`RateLimiter::available`] mutates, since reading it means first
/// folding in the time that has passed since the last call. Sharing one
/// limiter between threads is therefore an explicit decision, made by
/// wrapping it in [`Shared`](crate::Shared), rather than something that
/// happens by accident behind a `&self` method.
///
/// Implementing this trait for your own algorithm is enough to use it
/// with [`Shared`](crate::Shared) and, with
/// [`FromQuota`](crate::FromQuota), with [`Keyed`](crate::Keyed).
pub trait RateLimiter {
    /// The quota this limiter enforces.
    fn quota(&self) -> Quota;

    /// Takes `permits` if they are available.
    ///
    /// Permits are taken all-or-nothing: a refused request consumes
    /// nothing, so a large request that does not fit cannot partially
    /// drain the limiter and starve the small requests behind it.
    fn try_acquire_n(&mut self, permits: u32) -> Decision;

    /// Takes one permit if it is available.
    fn try_acquire(&mut self) -> Decision {
        self.try_acquire_n(1)
    }

    /// Reports what [`RateLimiter::try_acquire_n`] would answer, without
    /// taking anything.
    ///
    /// Note the `&mut self`: peeking still advances the limiter's view of
    /// time. What it does not do is consume permits.
    fn peek_n(&mut self, permits: u32) -> Decision;

    /// Reports what [`RateLimiter::try_acquire`] would answer, without
    /// taking anything.
    fn peek(&mut self) -> Decision {
        self.peek_n(1)
    }

    /// How many permits could be taken right now.
    fn available(&mut self) -> u32;

    /// Whether the limiter has fully recovered: no permits outstanding,
    /// so discarding it now and building a fresh one would be
    /// indistinguishable.
    ///
    /// This is what makes [`Keyed::retain_active`](crate::Keyed::retain_active)
    /// safe — evicting an idle per-key limiter cannot hand its key a
    /// permit it had not already earned back.
    fn is_idle(&mut self) -> bool;

    /// Discards all state, as if the limiter had just been constructed.
    fn reset(&mut self);
}
