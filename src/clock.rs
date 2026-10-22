//! Time, as an injected dependency.
//!
//! Every limiter in this crate reads time through [`Clock`] and never
//! calls [`std::time::Instant::now`] itself. That is not an abstraction
//! for its own sake — it is the single decision that makes the rest of
//! the crate testable.
//!
//! A rate limiter's interesting behaviour lives entirely in the shape of
//! time: how large a burst is absorbed at t=0, what is permitted one
//! nanosecond either side of a window boundary, how quickly permits
//! return after exhaustion. A test that has to `sleep` to reach those
//! points is slow, and worse, it is a coin flip — the assertion is really
//! "the OS scheduler woke us close enough to 100 ms". Those tests get
//! marked flaky and then get deleted, and the burst behaviour they were
//! guarding stops being checked at all.
//!
//! With [`ManualClock`], `advance(Duration::from_nanos(1))` is exact, so
//! the boundary cases can be asserted rather than approximated, and the
//! whole suite runs in milliseconds.
//!
//! ```
//! use std::time::Duration;
//! use ratelimit::{Clock, ManualClock};
//!
//! let clock = ManualClock::new();
//! assert_eq!(clock.now_nanos(), 0);
//! clock.advance(Duration::from_millis(250));
//! assert_eq!(clock.now_nanos(), 250_000_000);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A source of monotonic time, in nanoseconds since an arbitrary epoch.
///
/// The epoch has no meaning: only differences between two readings are
/// ever used, so a clock may start at zero, at process start, or anywhere
/// else. What matters is the two guarantees below.
///
/// # Contract
///
/// * **Monotonic.** A reading must never be smaller than an earlier
///   reading from the same clock.
/// * **Nanoseconds.** The unit is nanoseconds, so that a limiter's
///   arithmetic is exact integer arithmetic with no floating point and no
///   accumulated rounding drift.
///
/// The limiters do not *trust* the first guarantee — a clock that goes
/// backwards produces a saturating zero elapsed interval rather than a
/// wildly wrong permit count — but a clock that violates it will report
/// nonsense rates, and that is the implementation's fault, not the
/// limiter's.
pub trait Clock {
    /// Nanoseconds elapsed since this clock's epoch.
    fn now_nanos(&self) -> u64;
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now_nanos(&self) -> u64 {
        (**self).now_nanos()
    }
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now_nanos(&self) -> u64 {
        (**self).now_nanos()
    }
}

impl<C: Clock + ?Sized> Clock for Box<C> {
    fn now_nanos(&self) -> u64 {
        (**self).now_nanos()
    }
}

/// The real clock: [`Instant`] elapsed since the clock was created.
///
/// `Instant` is monotonic on every platform Rust supports, which is
/// exactly the contract [`Clock`] asks for. [`std::time::SystemTime`] is
/// deliberately not used: it can jump backwards when the machine syncs
/// with NTP, and a rate limiter reading a clock that jumps backwards
/// stops limiting.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Creates a clock whose epoch is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_nanos(&self) -> u64 {
        // A process would have to run for 584 years to overflow u64
        // nanoseconds. Saturating rather than wrapping keeps that
        // theoretical case monotonic instead of catastrophic.
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A clock that only moves when a test tells it to.
///
/// Shared by reference or inside an [`Arc`], including across threads:
/// the reading is a single [`AtomicU64`], so a limiter under test can be
/// hammered from many threads while time stands still — which is how the
/// concurrency tests in this crate assert an exact total permit count
/// instead of a range.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Decision, ManualClock, Quota, RateLimiter, TokenBucket};
///
/// let clock = ManualClock::new();
/// let quota = Quota::per_second(2).unwrap();
/// let mut bucket = TokenBucket::with_clock(quota, &clock);
///
/// assert!(bucket.try_acquire().is_allowed());
/// assert!(bucket.try_acquire().is_allowed());
/// assert!(!bucket.try_acquire().is_allowed());
///
/// clock.advance(Duration::from_millis(500));
/// assert!(bucket.try_acquire().is_allowed());
/// # let _: Decision = bucket.try_acquire();
/// ```
#[derive(Debug, Default)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// Creates a clock reading zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
        }
    }

    /// Creates a clock reading `nanos`.
    ///
    /// Useful for proving that a limiter depends on *differences* between
    /// readings and not on the epoch itself: every test in this crate
    /// that passes at zero must also pass starting from an arbitrary
    /// offset.
    #[must_use]
    pub fn starting_at(nanos: u64) -> Self {
        Self {
            nanos: AtomicU64::new(nanos),
        }
    }

    /// Moves the clock forward by `delta`.
    ///
    /// Saturates at [`u64::MAX`] rather than wrapping, so the clock can
    /// never violate the [`Clock`] monotonicity contract, even when a
    /// test deliberately advances it by an absurd amount.
    pub fn advance(&self, delta: Duration) {
        let delta = u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX);
        self.advance_nanos(delta);
    }

    /// Moves the clock forward by `nanos` nanoseconds.
    pub fn advance_nanos(&self, nanos: u64) {
        // A compare-and-swap loop rather than fetch_add, because
        // fetch_add wraps on overflow and wrapping time is the one thing
        // this type must not do.
        let mut current = self.nanos.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(nanos);
            match self.nanos.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Sets the clock to `nanos`, which must not be in the past.
    ///
    /// # Panics
    ///
    /// Panics if `nanos` is earlier than the current reading. Silently
    /// allowing a test clock to go backwards would let a test assert
    /// behaviour no real clock can produce.
    pub fn set_nanos(&self, nanos: u64) {
        // Check before storing: a panic must leave the clock where it
        // was, not already moved backwards for whoever catches it.
        let mut current = self.nanos.load(Ordering::SeqCst);
        loop {
            assert!(
                nanos >= current,
                "ManualClock::set_nanos({nanos}) would move time backwards from {current}"
            );
            match self.nanos.compare_exchange_weak(
                current,
                nanos,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::SeqCst)
    }
}

/// Converts a [`Duration`] to whole nanoseconds, saturating at
/// [`u64::MAX`].
///
/// Every public constructor funnels durations through this, so a caller
/// passing [`Duration::MAX`] gets a saturated quota rather than a panic
/// in release and a different panic in debug.
pub(crate) fn duration_to_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}
