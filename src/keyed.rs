//! One limiter per key, for per-client limits.

use std::collections::HashMap;
use std::hash::Hash;

use crate::clock::{Clock, SystemClock};
use crate::decision::{Decision, RateLimiter};
use crate::quota::Quota;

/// Builds a limiter from a quota and a clock.
///
/// Implemented by all four algorithms in this crate. It exists so
/// [`Keyed`] can create a limiter for a key it has not seen before
/// without knowing which algorithm it is holding; implement it for your
/// own limiter and [`Keyed`] works with that too.
pub trait FromQuota<C: Clock>: RateLimiter {
    /// Creates a fresh limiter enforcing `quota`, reading `clock`.
    fn from_quota(quota: Quota, clock: C) -> Self;
}

/// A limiter per key, created on first sight of the key.
///
/// The usual shape of a real rate limit: not "1000 requests per second"
/// but "10 requests per second *each*". Any algorithm in this crate can
/// fill it, so the choice of algorithm and the choice of per-key
/// bookkeeping stay independent.
///
/// # Memory, and the reason [`Keyed::retain_active`] exists
///
/// A key is remembered until it is evicted, so a naive per-IP limiter
/// grows without bound and is itself a denial-of-service vector — an
/// attacker sending one request from each of a million addresses makes
/// you allocate a million limiters. This type will not evict behind your
/// back (silent eviction under memory pressure would drop exactly the
/// state that is under attack), so call [`Keyed::retain_active`]
/// periodically.
///
/// It is safe to call at any moment, because it only drops limiters that
/// are *fully recovered*: a key whose limiter has no permits outstanding
/// is indistinguishable from a key that has never been seen, so dropping
/// it cannot hand anyone a permit they had not already earned back. Keys
/// that are actively being limited are exactly the ones it keeps.
///
/// ```
/// use std::time::Duration;
/// use ratelimit::{Gcra, Keyed, ManualClock, Quota, RateLimiter};
///
/// let clock = ManualClock::new();
/// let quota = Quota::per_second(2)?;
/// let mut limits: Keyed<&str, Gcra<&ManualClock>, &ManualClock> =
///     Keyed::with_clock(quota, &clock);
///
/// assert!(limits.try_acquire("10.0.0.1").is_allowed());
/// assert!(limits.try_acquire("10.0.0.1").is_allowed());
/// assert!(limits.try_acquire("10.0.0.1").is_denied());
///
/// // A different client has its own budget.
/// assert!(limits.try_acquire("10.0.0.2").is_allowed());
/// assert_eq!(limits.len(), 2);
///
/// // Once everyone has recovered, the map can be emptied.
/// clock.advance(Duration::from_secs(1));
/// limits.retain_active();
/// assert_eq!(limits.len(), 0);
/// # Ok::<(), ratelimit::QuotaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Keyed<K, L, C: Clock = SystemClock> {
    quota: Quota,
    clock: C,
    limiters: HashMap<K, L>,
}

impl<K, L> Keyed<K, L, SystemClock>
where
    K: Eq + Hash,
    L: FromQuota<SystemClock>,
{
    /// Creates an empty map of limiters driven by the real clock.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self::with_clock(quota, SystemClock::new())
    }
}

impl<K, L, C> Keyed<K, L, C>
where
    K: Eq + Hash,
    C: Clock + Clone,
    L: FromQuota<C>,
{
    /// Creates an empty map of limiters driven by `clock`.
    #[must_use]
    pub fn with_clock(quota: Quota, clock: C) -> Self {
        Self {
            quota,
            clock,
            limiters: HashMap::new(),
        }
    }

    /// The quota every key is held to.
    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.quota
    }

    /// Takes one permit for `key`.
    pub fn try_acquire(&mut self, key: K) -> Decision {
        self.try_acquire_n(key, 1)
    }

    /// Takes `permits` for `key`.
    pub fn try_acquire_n(&mut self, key: K, permits: u32) -> Decision {
        self.limiter(key).try_acquire_n(permits)
    }

    /// Reports what [`Keyed::try_acquire_n`] would answer for `key`,
    /// without taking anything.
    ///
    /// Note that peeking still creates the key's limiter if it is new —
    /// a fresh limiter is what the answer is about.
    pub fn peek_n(&mut self, key: K, permits: u32) -> Decision {
        self.limiter(key).peek_n(permits)
    }

    /// Permits currently available to `key`.
    pub fn available(&mut self, key: K) -> u32 {
        self.limiter(key).available()
    }

    /// The limiter for `key`, created if this is the first time the key
    /// has been seen.
    pub fn limiter(&mut self, key: K) -> &mut L {
        let quota = self.quota;
        // Borrowing two fields of `self` separately, so the clock can be
        // cloned lazily — only when a key is actually new.
        let clock = &self.clock;
        self.limiters
            .entry(key)
            .or_insert_with(|| L::from_quota(quota, clock.clone()))
    }

    /// The limiter for `key`, if the key has been seen.
    pub fn get(&mut self, key: &K) -> Option<&mut L> {
        self.limiters.get_mut(key)
    }

    /// Whether `key` has a limiter.
    pub fn contains_key(&self, key: &K) -> bool {
        self.limiters.contains_key(key)
    }

    /// How many keys are being tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.limiters.len()
    }

    /// Whether no keys are being tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.limiters.is_empty()
    }

    /// Forgets `key`, returning whether it was known.
    ///
    /// This gives the key a fresh full allowance, so it is an
    /// administrative action ("clear this customer's limit"), not a
    /// cleanup one. For cleanup use [`Keyed::retain_active`].
    pub fn forget(&mut self, key: &K) -> bool {
        self.limiters.remove(key).is_some()
    }

    /// Forgets every key.
    pub fn clear(&mut self) {
        self.limiters.clear();
    }

    /// Drops the limiters of every fully recovered key, returning how
    /// many were dropped.
    ///
    /// See the type documentation: this cannot change any decision,
    /// because a fully recovered limiter and a brand new one behave
    /// identically.
    pub fn retain_active(&mut self) -> usize {
        let before = self.limiters.len();
        self.limiters.retain(|_, limiter| !limiter.is_idle());
        before - self.limiters.len()
    }
}
