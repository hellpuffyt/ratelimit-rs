//! Fixed window: a counter that resets on a schedule.

use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::decision::{Decision, RateLimiter};
use crate::keyed::FromQuota;
use crate::quota::Quota;

/// A counter of permits used in the current window, reset at every
/// window boundary.
///
/// # What it permits, and why that is a problem
///
/// This is the limiter everyone writes first, and it is included here for
/// two honest reasons: it is genuinely the right choice sometimes, and it
/// is the baseline that makes [`SlidingWindow`](crate::SlidingWindow)'s
/// behaviour legible.
///
/// Its flaw has a name — the **boundary burst**. A client that spends its
/// whole budget at the end of one window and its whole budget at the
/// start of the next has sent `2 * count` permits inside one window's
/// worth of time, while never breaking the limit as the limiter defines
/// it. Configure 100 per minute and a determined client gets 200 in a
/// single minute, straddling 12:00:59 and 12:01:00.
///
/// That is not a rounding error, it is a factor of two, and it is
/// asserted rather than described: `tests/fixed_window.rs` walks a client
/// across a boundary and checks that exactly `2 * count` permits land in
/// one window's span, while the matching test on
/// [`SlidingWindow`](crate::SlidingWindow) checks that the same traffic
/// is refused.
///
/// Recovery has the same staircase shape: after exhaustion **nothing** is
/// permitted until the boundary, and then the entire budget is available
/// at once. Compare the token bucket, which returns one permit every
/// `emission_interval`.
///
/// # When to use it anyway
///
/// When the window boundary is part of the contract rather than an
/// implementation detail — a billing period, a daily free-tier
/// allowance, "1000 API calls per calendar month". There, resetting on a
/// schedule *is* the requirement, and the boundary burst is not a flaw
/// because the boundary is real. It is also the cheapest and easiest to
/// explain to users.
///
/// # Boundary alignment
///
/// Windows are aligned to the clock's epoch, not to the first request, so
/// two limiters sharing a clock agree on where the boundaries are. For
/// [`SystemClock`] the epoch is when the clock was created, so boundaries
/// are *not* aligned to wall-clock minutes — if you need a window that
/// resets on the calendar minute, drive this from a [`Clock`] whose
/// epoch is one.
///
/// The [`Quota`]'s burst is ignored: a window counter's burst allowance
/// is inherently its whole budget, since nothing stops all of it being
/// spent at one instant.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, FixedWindow, ManualClock, Quota, RateLimiter};
///
/// let clock = ManualClock::new();
/// let mut window = FixedWindow::with_clock(Quota::per_second(3)?, &clock);
///
/// // The whole budget, at one instant.
/// assert_eq!(window.try_acquire_n(3), Decision::Allow { remaining: 0 });
/// assert_eq!(
///     window.try_acquire(),
///     Decision::Deny { retry_after: Duration::from_secs(1) }
/// );
///
/// // Nothing is returned gradually; it all returns at the boundary.
/// clock.advance(Duration::from_millis(999));
/// assert!(window.try_acquire().is_denied());
/// clock.advance(Duration::from_millis(1));
/// assert_eq!(window.try_acquire_n(3), Decision::Allow { remaining: 0 });
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct FixedWindow<C: Clock = SystemClock> {
    quota: Quota,
    clock: C,
    /// Which window the counter belongs to: `now / period`.
    window_index: u64,
    /// Permits used in that window.
    used: u32,
}

impl FixedWindow<SystemClock> {
    /// Creates an empty window counter driven by the real clock.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self::with_clock(quota, SystemClock::new())
    }
}

impl<C: Clock> FixedWindow<C> {
    /// Creates an empty window counter driven by `clock`.
    #[must_use]
    pub fn with_clock(quota: Quota, clock: C) -> Self {
        let window_index = clock.now_nanos() / quota.period_nanos();
        Self {
            quota,
            clock,
            window_index,
            used: 0,
        }
    }

    /// The clock this limiter reads.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Permits used so far in the current window.
    pub fn used(&mut self) -> u32 {
        self.roll();
        self.used
    }

    /// How long until the current window ends.
    pub fn window_remaining(&self) -> Duration {
        let period = u128::from(self.quota.period_nanos());
        let now = u128::from(self.clock.now_nanos());
        // Measured to the end of the window the counter belongs to, which
        // is the current reading's window unless the clock went
        // backwards. In u128 because the end of the last window can lie
        // beyond `u64::MAX`.
        let index = (now / period).max(u128::from(self.window_index));
        let remaining = (index + 1) * period - now;
        Duration::from_nanos(u64::try_from(remaining).unwrap_or(u64::MAX))
    }

    /// Moves to the current window, clearing the counter if it changed.
    ///
    /// Any change of index clears — one window later or a thousand makes
    /// no difference, because a fixed window keeps no history at all.
    /// That absence of history is exactly what
    /// [`SlidingWindow`](crate::SlidingWindow) adds.
    ///
    /// A reading from an *earlier* window (a clock that went backwards)
    /// is ignored rather than treated as a new window: resetting the
    /// counter there would hand out a fresh budget for time that has
    /// already been spent.
    fn roll(&mut self) {
        let index = self.clock.now_nanos() / self.quota.period_nanos();
        if index > self.window_index {
            self.window_index = index;
            self.used = 0;
        }
    }

    fn apply(&mut self, permits: u32, commit: bool) -> Decision {
        self.roll();
        let limit = self.quota.count();
        if permits == 0 {
            return Decision::Allow {
                remaining: limit - self.used,
            };
        }
        if permits > limit {
            return Decision::Exceeds { capacity: limit };
        }
        match limit
            .checked_sub(self.used)
            .and_then(|l| l.checked_sub(permits))
        {
            Some(remaining) => {
                if commit {
                    self.used += permits;
                }
                Decision::Allow { remaining }
            }
            None => Decision::Deny {
                // The counter clears at the boundary and `permits <=
                // limit`, so the next window is guaranteed to satisfy
                // this request if nothing else takes the budget first.
                retry_after: self.window_remaining(),
            },
        }
    }
}

impl<C: Clock> RateLimiter for FixedWindow<C> {
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
        self.roll();
        self.quota.count() - self.used
    }

    fn is_idle(&mut self) -> bool {
        self.roll();
        self.used == 0
    }

    fn reset(&mut self) {
        self.window_index = self.clock.now_nanos() / self.quota.period_nanos();
        self.used = 0;
    }
}

impl<C: Clock> FromQuota<C> for FixedWindow<C> {
    fn from_quota(quota: Quota, clock: C) -> Self {
        Self::with_clock(quota, clock)
    }
}
