//! Sharing one limiter between threads.

use std::sync::{Mutex, MutexGuard};

use crate::decision::{Decision, RateLimiter};
use crate::quota::Quota;

/// A limiter behind a mutex, callable through `&self`.
///
/// [`RateLimiter`] takes `&mut self` because a limiter is mutable state.
/// That is deliberate: it makes sharing an explicit act rather than
/// something that happens by accident, and it keeps the single-threaded
/// case — one limiter owned by one task — free of any synchronisation at
/// all.
///
/// When sharing is what you want, this is it. It is a plain
/// [`std::sync::Mutex`] and no more: the critical section is a handful of
/// integer operations with no allocation and no syscall, so an atomic
/// compare-and-swap design would buy contention behaviour that almost no
/// caller can measure, at the cost of an algorithm that is hard to
/// verify. If you have measured that this mutex is your bottleneck, shard
/// it — several `Shared` limiters, each with `1/n` of the quota, selected
/// by a hash of the key.
///
/// # Poisoning
///
/// Lock poisoning is deliberately ignored. Poisoning exists to stop
/// callers observing data left half-updated by a panic, and no code path
/// here can panic while holding the lock — the critical section is
/// integer arithmetic on state that has no cross-field invariant to
/// break. Propagating a poison error would mean one unrelated panic
/// anywhere in the process permanently bricks rate limiting, which is a
/// far worse outcome than the one poisoning protects against.
///
/// ```
/// use std::sync::Arc;
/// use std::thread;
/// use ratelimit::{ManualClock, Quota, Shared, TokenBucket};
///
/// let clock = Arc::new(ManualClock::new());
/// let quota = Quota::per_second(100)?;
/// let limiter = Arc::new(Shared::new(TokenBucket::with_clock(quota, clock.clone())));
///
/// let mut handles = Vec::new();
/// for _ in 0..8 {
///     let limiter = Arc::clone(&limiter);
///     handles.push(thread::spawn(move || {
///         (0..50).filter(|_| limiter.try_acquire().is_allowed()).count()
///     }));
/// }
/// let allowed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
///
/// // 400 attempts, a frozen clock, a budget of 100: exactly 100 get
/// // through, however the threads interleaved.
/// assert_eq!(allowed, 100);
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Default)]
pub struct Shared<L> {
    inner: Mutex<L>,
}

impl<L> Shared<L> {
    /// Wraps `limiter`.
    #[must_use]
    pub const fn new(limiter: L) -> Self {
        Self {
            inner: Mutex::new(limiter),
        }
    }

    /// Runs `f` with exclusive access to the limiter.
    ///
    /// The escape hatch for anything the forwarding methods do not cover
    /// — algorithm-specific inspection, or several operations that must
    /// be atomic with respect to each other. Also the way to use
    /// [`Keyed`](crate::Keyed) from several threads, since its methods
    /// need the key.
    pub fn with<R>(&self, f: impl FnOnce(&mut L) -> R) -> R {
        f(&mut self.lock())
    }

    /// Unwraps the limiter.
    pub fn into_inner(self) -> L {
        self.inner.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks, recovering from poisoning. See the type documentation.
    fn lock(&self) -> MutexGuard<'_, L> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<L: RateLimiter> Shared<L> {
    /// The quota being enforced.
    pub fn quota(&self) -> Quota {
        self.lock().quota()
    }

    /// Takes one permit if it is available.
    pub fn try_acquire(&self) -> Decision {
        self.lock().try_acquire()
    }

    /// Takes `permits` if they are available.
    pub fn try_acquire_n(&self, permits: u32) -> Decision {
        self.lock().try_acquire_n(permits)
    }

    /// Reports what [`Shared::try_acquire`] would answer, without taking
    /// anything.
    ///
    /// Inherently a stale answer the moment it is returned, since another
    /// thread may acquire in between. Use it for reporting, never to
    /// decide whether to call [`Shared::try_acquire`].
    pub fn peek(&self) -> Decision {
        self.lock().peek()
    }

    /// Reports what [`Shared::try_acquire_n`] would answer, without
    /// taking anything.
    pub fn peek_n(&self, permits: u32) -> Decision {
        self.lock().peek_n(permits)
    }

    /// How many permits could be taken right now.
    pub fn available(&self) -> u32 {
        self.lock().available()
    }

    /// Whether the limiter has fully recovered.
    pub fn is_idle(&self) -> bool {
        self.lock().is_idle()
    }

    /// Discards all state.
    pub fn reset(&self) {
        self.lock().reset();
    }
}
