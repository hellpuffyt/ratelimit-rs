//! GCRA: the generic cell rate algorithm, a leaky bucket expressed as a
//! single scheduled instant.

use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::decision::{Decision, RateLimiter};
use crate::keyed::FromQuota;
use crate::quota::Quota;

/// A virtual-scheduling limiter: one timestamp, one comparison.
///
/// # What it permits
///
/// GCRA tracks a single instant, the **theoretical arrival time** (TAT):
/// when the next permit would fall due if traffic were perfectly paced at
/// one per [`Quota::emission_interval`]. A request is allowed when the
/// TAT is no further into the future than the burst tolerance
/// `burst * emission_interval`; allowing it pushes the TAT forward by one
/// interval per permit.
///
/// So a caller may run up to `burst` permits *ahead of schedule*, and
/// then must wait for the schedule to catch up.
///
/// # It permits exactly what the token bucket permits
///
/// This is worth stating plainly, because rate-limiting write-ups
/// routinely imply otherwise — that GCRA is "smoother" or "stricter" than
/// a token bucket. It is not. A token bucket with continuous refill and a
/// GCRA with the same quota are the same algorithm looked at from two
/// ends: credit accumulated since the last permit, versus the time at
/// which the next permit falls due. Credit `c` and delay `d` are related
/// by `c = capacity - d`, and every decision follows from that.
///
/// This crate does not ask you to take that on trust.
/// `tests/comparison.rs` drives a [`TokenBucket`](crate::TokenBucket) and
/// a [`Gcra`] over the same pseudo-random schedule of thousands of
/// requests and asserts that **every** decision — allow, deny, and the
/// exact `retry_after` — is identical. If a future change to either
/// implementation breaks the equivalence, that test fails.
///
/// The equivalence holds for any clock that honours the [`Clock`]
/// contract. It does *not* survive a clock that runs backwards, and the
/// test suite pins that difference too: the bucket keeps the credit it
/// had accumulated, while GCRA refuses everything until the clock
/// catches up with the schedule it already recorded. Both refuse to mint
/// permits, which is the property that matters; they simply fail
/// differently.
///
/// The differences that are real:
///
/// * **State.** GCRA is one `u64`. The token bucket is two, because it
///   must remember when its credit was last recomputed. At a million
///   tracked keys that is 8 MB against 16 MB.
/// * **Framing.** GCRA already knows *when* the next permit falls due,
///   which is the natural shape for a scheduler that wants to sleep
///   until a permit exists rather than poll. See
///   [`Gcra::next_permit_in`].
/// * **Convention.** GCRA's tolerance is usually configured as "how far
///   ahead of schedule may a client get", which is why `with_burst(1)` —
///   a strict pacer that never allows two permits at the same instant —
///   reads naturally here and looks like a crippled bucket there.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, Gcra, ManualClock, Quota, RateLimiter};
///
/// let clock = ManualClock::new();
/// // A strict pacer: 5 per second, never two at once.
/// let quota = Quota::per_second(5)?.with_burst(1)?;
/// let mut gcra = Gcra::with_clock(quota, &clock);
///
/// assert!(gcra.try_acquire().is_allowed());
/// assert_eq!(
///     gcra.try_acquire(),
///     Decision::Deny { retry_after: Duration::from_millis(200) }
/// );
///
/// clock.advance(Duration::from_millis(200));
/// assert!(gcra.try_acquire().is_allowed());
/// // Waiting ten times as long does not bank ten permits: the tolerance
/// // is one, so it is one.
/// clock.advance(Duration::from_secs(2));
/// assert!(gcra.try_acquire().is_allowed());
/// assert!(gcra.try_acquire().is_denied());
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Gcra<C: Clock = SystemClock> {
    quota: Quota,
    clock: C,
    /// The theoretical arrival time: the clock reading at which the
    /// schedule has caught up with the permits already issued. A reading
    /// in the past means the limiter is fully recovered.
    tat_nanos: u64,
}

impl Gcra<SystemClock> {
    /// Creates a limiter with its full burst tolerance available,
    /// driven by the real clock.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self::with_clock(quota, SystemClock::new())
    }
}

impl<C: Clock> Gcra<C> {
    /// Creates a limiter with its full burst tolerance available, driven
    /// by `clock`.
    #[must_use]
    pub fn with_clock(quota: Quota, clock: C) -> Self {
        let tat_nanos = clock.now_nanos();
        Self {
            quota,
            clock,
            tat_nanos,
        }
    }

    /// Creates a limiter with **no** tolerance available: the schedule
    /// starts already `burst` permits ahead.
    ///
    /// The counterpart of
    /// [`TokenBucket::empty_with_clock`](crate::TokenBucket::empty_with_clock),
    /// and useful for the same reason — a limiter rebuilt after a restart
    /// should not hand out a free burst.
    #[must_use]
    pub fn drained_with_clock(quota: Quota, clock: C) -> Self {
        let tat_nanos = clock.now_nanos().saturating_add(quota.capacity_nanos());
        Self {
            quota,
            clock,
            tat_nanos,
        }
    }

    /// The clock this limiter reads.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The theoretical arrival time, as a raw clock reading.
    ///
    /// Exposed because it is the entire state of the limiter: persist
    /// this one number and a limiter can be reconstructed exactly. A
    /// reading in the past means fully recovered.
    #[must_use]
    pub const fn tat_nanos(&self) -> u64 {
        self.tat_nanos
    }

    /// How long until one permit would be available, or
    /// [`Duration::ZERO`] if one is available now.
    ///
    /// The point of GCRA in a scheduler: rather than polling
    /// [`RateLimiter::try_acquire`] and backing off, a caller can sleep
    /// for exactly this long and then take the permit.
    #[must_use]
    pub fn next_permit_in(&self) -> Duration {
        self.delay_for(1)
    }

    /// How long until `permits` would be available, ignoring whether the
    /// limiter could ever hold that many.
    fn delay_for(&self, permits: u32) -> Duration {
        let now = self.clock.now_nanos();
        let tat = self.tat_nanos.max(now);
        // u128, so a schedule running past the end of the clock's range
        // reports a long wait instead of a saturated, too-short one.
        let cost = u128::from(permits) * u128::from(self.quota.emission_interval_nanos());
        let delay = u128::from(tat - now) + cost;
        let wait = delay.saturating_sub(u128::from(self.quota.capacity_nanos()));
        Duration::from_nanos(u64::try_from(wait).unwrap_or(u64::MAX))
    }

    fn apply(&mut self, permits: u32, commit: bool) -> Decision {
        let now = self.clock.now_nanos();
        // A TAT in the past means the schedule has caught up; clamping it
        // to `now` is what stops arbitrarily long idle periods from
        // banking arbitrarily many permits. This is also the only place
        // a backwards-moving clock could do damage, and the clamp
        // absorbs it: the TAT is never dragged backwards.
        let tat = self.tat_nanos.max(now);
        if permits == 0 {
            return Decision::Allow {
                remaining: Self::permits_from_delay(tat - now, self.quota),
            };
        }
        if permits > self.quota.burst() {
            return Decision::Exceeds {
                capacity: self.quota.burst(),
            };
        }
        let cost = u64::from(permits) * self.quota.emission_interval_nanos();
        let Some(new_tat) = tat.checked_add(cost) else {
            // The schedule would run past the last representable clock
            // reading (over 584 years after the clock's epoch). Saturating
            // here would store the same TAT again and again and admit
            // everything; refusing is the only answer that keeps the
            // limit, and no amount of waiting on this clock can change it.
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        };
        let delay = new_tat - now;
        if delay <= self.quota.capacity_nanos() {
            if commit {
                self.tat_nanos = new_tat;
            }
            Decision::Allow {
                remaining: Self::permits_from_delay(delay, self.quota),
            }
        } else {
            Decision::Deny {
                retry_after: Duration::from_nanos(delay - self.quota.capacity_nanos()),
            }
        }
    }

    /// How many permits are left when the schedule runs `delay`
    /// nanoseconds ahead of now.
    fn permits_from_delay(delay_nanos: u64, quota: Quota) -> u32 {
        let slack = quota.capacity_nanos().saturating_sub(delay_nanos);
        let permits = slack / quota.emission_interval_nanos();
        u32::try_from(permits)
            .unwrap_or(u32::MAX)
            .min(quota.burst())
    }
}

impl<C: Clock> RateLimiter for Gcra<C> {
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
        let now = self.clock.now_nanos();
        let delay = self.tat_nanos.saturating_sub(now);
        Self::permits_from_delay(delay, self.quota)
    }

    fn is_idle(&mut self) -> bool {
        self.tat_nanos <= self.clock.now_nanos()
    }

    fn reset(&mut self) {
        self.tat_nanos = self.clock.now_nanos();
    }
}

impl<C: Clock> FromQuota<C> for Gcra<C> {
    fn from_quota(quota: Quota, clock: C) -> Self {
        Self::with_clock(quota, clock)
    }
}
