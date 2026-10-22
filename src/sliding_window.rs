//! Sliding window: a fixed window that pays attention to the one before
//! it.

use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::decision::{Decision, RateLimiter};
use crate::keyed::FromQuota;
use crate::quota::Quota;

/// A weighted sliding-window counter: two counters and one multiply.
///
/// # What it permits
///
/// [`FixedWindow`](crate::FixedWindow) forgets the previous window
/// entirely at each boundary, which is what lets a client take `2 *
/// count` permits across one. This limiter keeps the previous window's
/// count and charges a *fraction* of it against the current budget — the
/// fraction of the previous window still inside the trailing window of
/// length `period` that ends now:
///
/// ```text
/// estimate = floor(previous * (period - offset) / period) + current
/// ```
///
/// where `offset` is how far into the current window the clock is. One
/// nanosecond after a boundary, essentially the whole previous count
/// still counts; a full period later, none of it does. The budget is
/// released continuously across the window rather than all at once, so
/// the boundary burst is gone: `tests/sliding_window.rs` runs the same
/// straddling traffic that gets `2 * count` past a fixed window and
/// asserts it is refused here.
///
/// # It is an estimate, and that is a real trade
///
/// The exact answer requires remembering *when* each permit was taken —
/// a sliding window log, which costs memory proportional to the limit per
/// key and is the thing this approximation exists to avoid. What is
/// stored here instead is two counters and an index, regardless of the
/// limit.
///
/// The price is that the previous window's permits are assumed to be
/// spread evenly through it, which is not how traffic arrives. Two
/// consequences, both asserted in the tests rather than left as folklore:
///
/// * **It refuses requests an exact log would allow.** Permits taken at
///   the *start* of the previous window are already older than `period`
///   but are still partly charged. Measured: a client sending at exactly
///   its permitted rate loses one request per window, and ten permits
///   taken at t=0 still count five against the window ending at t=1.5s.
///   Erring this way is the safe direction for a limiter, but it is not
///   free.
/// * **Its worst case is still `2 * count`, but it has to be aimed.**
///   Spend the whole budget in the last instant of a window, where the
///   weighting discounts it fastest, and `2 * count` fits into one
///   window-length span — the same as a fixed window. The difference is
///   how hard that is to hit: a client that simply retries as fast as it
///   can gets **exactly `count`** here, and a clean `2 * count` from a
///   fixed window. Both numbers are measured in
///   `tests/sliding_window.rs`, not estimated.
///
/// If a hard guarantee for every span is the requirement rather than a
/// good approximation of one, no counter-based limiter provides it;
/// use [`TokenBucket`](crate::TokenBucket) or [`Gcra`](crate::Gcra) with
/// a small burst, which bound any span at `burst + count`.
///
/// # When to use it
///
/// When the limit is a promise about a *named window* — "600 requests per
/// minute" in your published API docs — and you want that promise to hold
/// for every minute-long span, not just the ones that start on a
/// boundary. Users understand windows; a token bucket's burst allowance
/// takes a paragraph to explain.
///
/// Prefer a [`TokenBucket`](crate::TokenBucket) or [`Gcra`](crate::Gcra)
/// when you care about *pacing*. This limiter will happily let the whole
/// budget go in one instant and then release it back gradually; a bucket
/// enforces spacing between permits.
///
/// The [`Quota`]'s burst is ignored, for the same reason as
/// [`FixedWindow`](crate::FixedWindow): a counter's burst allowance is
/// its whole budget.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, ManualClock, Quota, RateLimiter, SlidingWindow};
///
/// let clock = ManualClock::new();
/// let mut window = SlidingWindow::with_clock(Quota::per_second(10)?, &clock);
///
/// // Spend the whole budget at the end of one window...
/// clock.advance(Duration::from_millis(900));
/// assert_eq!(window.try_acquire_n(10), Decision::Allow { remaining: 0 });
///
/// // ...cross the boundary. A fixed window would hand over the whole
/// // budget again here; this one still charges 90% of the previous
/// // window, so exactly one permit is free.
/// clock.advance(Duration::from_millis(200));
/// assert!(window.try_acquire_n(2).is_denied());
/// assert_eq!(window.try_acquire(), Decision::Allow { remaining: 0 });
///
/// // The rest returns gradually as the old window slides out.
/// clock.advance(Duration::from_millis(500));
/// assert_eq!(window.available(), 5);
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SlidingWindow<C: Clock = SystemClock> {
    quota: Quota,
    clock: C,
    /// Which window `current` belongs to: `now / period`.
    window_index: u64,
    /// Permits used in the window before `window_index`.
    previous: u32,
    /// Permits used in `window_index` so far.
    current: u32,
}

impl SlidingWindow<SystemClock> {
    /// Creates an empty sliding-window counter driven by the real clock.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self::with_clock(quota, SystemClock::new())
    }
}

impl<C: Clock> SlidingWindow<C> {
    /// Creates an empty sliding-window counter driven by `clock`.
    #[must_use]
    pub fn with_clock(quota: Quota, clock: C) -> Self {
        let window_index = clock.now_nanos() / quota.period_nanos();
        Self {
            quota,
            clock,
            window_index,
            previous: 0,
            current: 0,
        }
    }

    /// The clock this limiter reads.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The weighted estimate of permits used in the trailing `period`.
    ///
    /// This is the number compared against the limit, exposed because it
    /// is the whole behaviour of the algorithm and because a
    /// `X-RateLimit-Used` header wants it.
    pub fn weighted_used(&mut self) -> u32 {
        self.roll();
        self.estimate()
    }

    /// Moves to the current window, ageing the counters.
    ///
    /// Exactly one window later, the current count becomes the previous
    /// count. More than one window later, both are stale by definition —
    /// nothing in them is inside the trailing window any more — so both
    /// clear.
    ///
    /// A reading from an *earlier* window (a clock that went backwards)
    /// never rolls: clearing the counters there would hand out a fresh
    /// budget for time that has already been spent.
    fn roll(&mut self) {
        let index = self.clock.now_nanos() / self.quota.period_nanos();
        if index <= self.window_index {
            return;
        }
        if index - self.window_index == 1 {
            self.previous = self.current;
        } else {
            self.previous = 0;
        }
        self.current = 0;
        self.window_index = index;
    }

    /// How far into the current window the clock is.
    ///
    /// When the clock has gone backwards into an earlier window, the
    /// answer is zero: the start of the window the counters belong to,
    /// where the previous window weighs the most. That is the most
    /// conservative reading available, so a backwards clock can only
    /// make this limiter stricter, never looser.
    fn offset(&self) -> u64 {
        let now = self.clock.now_nanos();
        let period = self.quota.period_nanos();
        if now / period < self.window_index {
            0
        } else {
            now % period
        }
    }

    /// The weighted share of the previous window still inside the
    /// trailing window.
    ///
    /// Computed in `u128` because `previous * period` can exceed `u64`
    /// for a long period, and floored, which is the conventional choice:
    /// it errs towards permitting rather than towards charging a client
    /// for a fraction of a request it did not make.
    fn weighted_previous(&self) -> u32 {
        let period = self.quota.period_nanos();
        let carried = u128::from(self.previous) * u128::from(period - self.offset());
        u32::try_from(carried / u128::from(period)).unwrap_or(u32::MAX)
    }

    fn estimate(&self) -> u32 {
        self.weighted_previous().saturating_add(self.current)
    }

    /// The smallest offset into a window at which a previous count of
    /// `previous` weighs no more than `room`.
    ///
    /// Solves `floor(previous * (period - offset) / period) <= room` for
    /// the smallest `offset`, in integer arithmetic. Used to answer
    /// `retry_after` exactly rather than telling the caller to come back
    /// at the next boundary and hope.
    fn offset_when_weight_fits(previous: u32, period: u64, room: u32) -> u64 {
        if previous == 0 {
            return 0;
        }
        // floor(p * m / period) <= room  <=>  p * m < (room + 1) * period,
        // so the largest surviving span `m = period - offset` is
        // floor(((room + 1) * period - 1) / p).
        let numerator = (u128::from(room) + 1) * u128::from(period) - 1;
        let span = numerator / u128::from(previous);
        let span = u64::try_from(span.min(u128::from(period))).unwrap_or(period);
        period - span
    }

    /// How long until `permits` would fit, given the current counters.
    fn retry_after(&self, permits: u32) -> Duration {
        let period = self.quota.period_nanos();
        let limit = self.quota.count();
        let offset = self.offset();
        let nanos = if let Some(room) = limit
            .checked_sub(self.current)
            .and_then(|room| room.checked_sub(permits))
        {
            // The current window's own count leaves room, so the wait is
            // however long it takes the previous window to age out far
            // enough.
            Self::offset_when_weight_fits(self.previous, period, room) - offset
        } else {
            // Not even the boundary is enough on its own: wait for it,
            // and then for this window's count — which becomes the
            // *previous* count there — to age out in turn.
            // Saturating: with a period near `u64::MAX` the sum can exceed
            // the clock's range, and wrapping would advertise a tiny wait.
            let room = limit - permits;
            (period - offset).saturating_add(Self::offset_when_weight_fits(
                self.current,
                period,
                room,
            ))
        };
        Duration::from_nanos(nanos)
    }

    fn apply(&mut self, permits: u32, commit: bool) -> Decision {
        self.roll();
        let limit = self.quota.count();
        if permits == 0 {
            return Decision::Allow {
                remaining: limit.saturating_sub(self.estimate()),
            };
        }
        if permits > limit {
            return Decision::Exceeds { capacity: limit };
        }
        let estimate = self.estimate();
        match limit
            .checked_sub(estimate)
            .and_then(|room| room.checked_sub(permits))
        {
            Some(remaining) => {
                if commit {
                    self.current += permits;
                }
                Decision::Allow { remaining }
            }
            None => Decision::Deny {
                retry_after: self.retry_after(permits),
            },
        }
    }
}

impl<C: Clock> RateLimiter for SlidingWindow<C> {
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
        self.quota.count().saturating_sub(self.estimate())
    }

    fn is_idle(&mut self) -> bool {
        self.roll();
        self.estimate() == 0
    }

    fn reset(&mut self) {
        self.window_index = self.clock.now_nanos() / self.quota.period_nanos();
        self.previous = 0;
        self.current = 0;
    }
}

impl<C: Clock> FromQuota<C> for SlidingWindow<C> {
    fn from_quota(quota: Quota, clock: C) -> Self {
        Self::with_clock(quota, clock)
    }
}
