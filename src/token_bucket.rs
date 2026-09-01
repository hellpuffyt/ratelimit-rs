//! Token bucket: a pool of permits that refills continuously.

use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::decision::{Decision, RateLimiter};
use crate::keyed::FromQuota;
use crate::quota::Quota;

/// A bucket of `burst` permits that refills at the quota's rate.
///
/// # What it permits
///
/// The bucket starts full. Every permit taken removes one, and permits
/// return continuously — not in a lump once per period — at one per
/// [`Quota::emission_interval`]. So a caller that has been quiet may take
/// up to `burst` permits in a single instant, and after that is paced at
/// the sustained rate. Over any interval `d`, the most permits obtainable
/// is `burst + d / emission_interval`, and that bound is asserted
/// directly in `tests/token_bucket.rs`.
///
/// This is the right default for most APIs: bursts are what real clients
/// produce (a page load fires eight requests at once), and allowing them
/// while still holding the long-run average is precisely what a token
/// bucket does.
///
/// # How it is implemented
///
/// The obvious implementation stores a token count and a timer that adds
/// tokens on a schedule. This one stores neither. State is two `u64`s:
/// nanoseconds of accumulated credit, and when it was last updated.
/// Refilling is `credit += now - last`, capped; taking `n` permits is
/// `credit -= n * emission_interval`.
///
/// That is worth the small mental cost twice over. There is no background
/// task and no timer to schedule, so an idle limiter costs exactly
/// nothing, and refill is *continuous* rather than quantised to whatever
/// tick the timer used — a caller never has to wait for the top of a
/// tick to get the permit it has already earned. Everything is integer
/// arithmetic, so a million permits later there is no accumulated
/// floating-point drift.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, ManualClock, Quota, RateLimiter, TokenBucket};
///
/// let clock = ManualClock::new();
/// // 10/second sustained, but up to 3 at once.
/// let quota = Quota::per_second(10)?.with_burst(3)?;
/// let mut bucket = TokenBucket::with_clock(quota, &clock);
///
/// // The burst is absorbed at a single instant.
/// assert!(bucket.try_acquire().is_allowed());
/// assert!(bucket.try_acquire().is_allowed());
/// assert!(bucket.try_acquire().is_allowed());
/// assert_eq!(
///     bucket.try_acquire(),
///     Decision::Deny { retry_after: Duration::from_millis(100) }
/// );
///
/// // ...and then permits drip back, one every 100 ms.
/// clock.advance(Duration::from_millis(100));
/// assert_eq!(bucket.try_acquire(), Decision::Allow { remaining: 0 });
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct TokenBucket<C: Clock = SystemClock> {
    quota: Quota,
    clock: C,
    /// Credit in nanoseconds, between 0 and `quota.capacity_nanos()`.
    /// One permit costs `quota.emission_interval_nanos()` of it.
    credit_nanos: u64,
    /// The clock reading at which `credit_nanos` was last correct.
    last_nanos: u64,
}

impl TokenBucket<SystemClock> {
    /// Creates a full bucket driven by the real clock.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self::with_clock(quota, SystemClock::new())
    }
}

impl<C: Clock> TokenBucket<C> {
    /// Creates a full bucket driven by `clock`.
    #[must_use]
    pub fn with_clock(quota: Quota, clock: C) -> Self {
        let last_nanos = clock.now_nanos();
        Self {
            quota,
            clock,
            credit_nanos: quota.capacity_nanos(),
            last_nanos,
        }
    }

    /// Creates an *empty* bucket driven by `clock`.
    ///
    /// A full bucket is the right default — a client that has never been
    /// seen has not used anything — but an empty one is occasionally what
    /// you want: a limiter restored after a restart, where crediting
    /// every client a fresh burst would let a restart loop multiply the
    /// permitted rate.
    #[must_use]
    pub fn empty_with_clock(quota: Quota, clock: C) -> Self {
        let last_nanos = clock.now_nanos();
        Self {
            quota,
            clock,
            credit_nanos: 0,
            last_nanos,
        }
    }

    /// The clock this bucket reads.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Folds elapsed time into the credit pool.
    ///
    /// The clock is *not* trusted to be monotonic: a reading earlier than
    /// the last one produces a saturating zero, which leaves the bucket
    /// untouched rather than crediting it with a nonsense interval. The
    /// stored reading is only moved forward, so one bad reading cannot
    /// poison later ones.
    fn refill(&mut self) {
        let now = self.clock.now_nanos();
        let elapsed = now.saturating_sub(self.last_nanos);
        if elapsed == 0 {
            return;
        }
        self.credit_nanos = self
            .credit_nanos
            .saturating_add(elapsed)
            .min(self.quota.capacity_nanos());
        self.last_nanos = now;
    }

    fn apply(&mut self, permits: u32, commit: bool) -> Decision {
        self.refill();
        if permits == 0 {
            // Asking for nothing is always granted, and consumes
            // nothing. Callers that compute a cost dynamically get a
            // sensible answer instead of a special case.
            return Decision::Allow {
                remaining: self.permits_available(),
            };
        }
        if permits > self.quota.burst() {
            return Decision::Exceeds {
                capacity: self.quota.burst(),
            };
        }
        // `permits <= burst`, so this product is at most
        // `capacity_nanos`, which `Quota` guarantees fits in u64.
        let cost = u64::from(permits) * self.quota.emission_interval_nanos();
        match self.credit_nanos.checked_sub(cost) {
            Some(left) => {
                if commit {
                    self.credit_nanos = left;
                }
                Decision::Allow {
                    remaining: Self::permits_in(left, self.quota),
                }
            }
            None => Decision::Deny {
                // Credit accrues at exactly one nanosecond per
                // nanosecond, so the shortfall in credit *is* the wait,
                // with no conversion and no rounding.
                retry_after: Duration::from_nanos(cost - self.credit_nanos),
            },
        }
    }

    fn permits_available(&self) -> u32 {
        Self::permits_in(self.credit_nanos, self.quota)
    }

    fn permits_in(credit_nanos: u64, quota: Quota) -> u32 {
        let permits = credit_nanos / quota.emission_interval_nanos();
        // Bounded by `burst` because credit is capped at
        // `burst * emission_interval`, so this cast cannot truncate.
        u32::try_from(permits)
            .unwrap_or(u32::MAX)
            .min(quota.burst())
    }
}

impl<C: Clock> RateLimiter for TokenBucket<C> {
    fn quota(&self) -> Quota {
        self.quota
    }

    fn try_acquire_n(&mut self, permits: u32) -> Decision {
        self.apply(permits, true)
    }

    fn peek_n(&mut self, permits: u32) -> Decision {
        self.apply(permits, false)
    }

    fn available(&mut self) -> u32 {
        self.refill();
        self.permits_available()
    }

    fn is_idle(&mut self) -> bool {
        self.refill();
        self.credit_nanos >= self.quota.capacity_nanos()
    }

    fn reset(&mut self) {
        self.credit_nanos = self.quota.capacity_nanos();
        self.last_nanos = self.clock.now_nanos();
    }
}

impl<C: Clock> FromQuota<C> for TokenBucket<C> {
    fn from_quota(quota: Quota, clock: C) -> Self {
        Self::with_clock(quota, clock)
    }
}
