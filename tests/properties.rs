//! Property tests: every limiter checked against an independent
//! reference model, over many reproducible pseudo-random schedules and
//! quotas — including the extreme ends of the `u64` nanosecond range.
//!
//! The reference models are written to be *obviously* correct rather
//! than efficient: the window models recompute their counts from a full
//! log of grant times instead of rolling counters, and every model does
//! its arithmetic in `u128` so it cannot overflow where the real
//! implementation might.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use ratelimit::{
    Clock, Decision, FixedWindow, Gcra, Quota, RateLimiter, SlidingWindow, TokenBucket,
};

/// A deterministic pseudo-random source (PCG-style LCG). Reproducible on
/// every machine, so a failure can be re-run with the printed seed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// A clock owned by value, so cloning a limiter clones its clock too.
/// That is what lets a test fork a limiter, move the fork's time, and
/// ask it a question without disturbing the original.
#[derive(Clone, Debug)]
struct CellClock(Cell<u64>);

impl CellClock {
    fn at(nanos: u64) -> Self {
        Self(Cell::new(nanos))
    }
    fn advance(&self, nanos: u64) {
        self.0.set(self.0.get().saturating_add(nanos));
    }
    fn set(&self, nanos: u64) {
        self.0.set(nanos);
    }
}

impl Clock for CellClock {
    fn now_nanos(&self) -> u64 {
        self.0.get()
    }
}

/// Access to the owned clock of any of the four limiters.
trait Probe: RateLimiter + Clone {
    fn build(quota: Quota, clock: CellClock) -> Self;
    fn time(&self) -> &CellClock;
}

macro_rules! probe {
    ($ty:ident) => {
        impl Probe for $ty<CellClock> {
            fn build(quota: Quota, clock: CellClock) -> Self {
                $ty::with_clock(quota, clock)
            }
            fn time(&self) -> &CellClock {
                self.clock()
            }
        }
    };
}
probe!(TokenBucket);
probe!(Gcra);
probe!(SlidingWindow);
probe!(FixedWindow);

fn random_quota(rng: &mut Lcg) -> Quota {
    let count = u32::try_from(1 + rng.below(20)).expect("small");
    // A mix of period scales, from a handful of nanoseconds (where
    // integer division is at its coarsest) to whole seconds.
    let period = match rng.below(3) {
        0 => Duration::from_nanos(u64::from(count) + rng.below(40)),
        1 => Duration::from_micros(1 + rng.below(5_000)),
        _ => Duration::from_millis(1 + rng.below(2_000)),
    };
    let quota = Quota::new(count, period).expect("valid quota");
    let burst = u32::try_from(1 + rng.below(30)).expect("small");
    quota.with_burst(burst).expect("valid burst")
}

fn random_step(rng: &mut Lcg, quota: Quota) -> u64 {
    let period = u64::try_from(quota.period().as_nanos()).expect("fits");
    match rng.below(6) {
        0 => 0,
        1 => 1,
        2 => rng.below(period / 4 + 1),
        3 => rng.below(period + 1),
        4 => period - rng.below(2).min(period),
        _ => rng.below(3 * period + 1),
    }
}

// --- retry_after is exact ------------------------------------------------

/// `Decision` documents `retry_after` as exact for every limiter in the
/// crate: waiting that long (with no other traffic) is sufficient, and
/// waiting one nanosecond less is not.
fn retry_after_is_exact<L: Probe>(name: &str) {
    for seed in 0..300_u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9) ^ 0xA5A5);
        let quota = random_quota(&mut rng);
        let mut limiter = L::build(quota, CellClock::at(rng.next() << 20));
        let cap = match limiter.try_acquire_n(u32::MAX) {
            Decision::Exceeds { capacity } => capacity,
            other => panic!("{name}: u32::MAX permits gave {other:?}"),
        };
        for step in 0..200 {
            let permits = u32::try_from(rng.below(u64::from(cap) + 1)).expect("fits");
            if let Decision::Deny { retry_after } = limiter.try_acquire_n(permits) {
                let wait = u64::try_from(retry_after.as_nanos()).expect("fits");
                assert!(wait > 0, "{name} seed {seed} step {step}: zero wait");

                let enough = limiter.clone();
                enough.time().advance(wait);
                let mut enough = enough;
                assert!(
                    enough.peek_n(permits).is_allowed(),
                    "{name} seed {seed} step {step} {quota:?}: still denied after \
                     the advertised {wait}ns for {permits} permits"
                );

                let mut short = limiter.clone();
                short.time().advance(wait - 1);
                assert!(
                    short.peek_n(permits).is_denied(),
                    "{name} seed {seed} step {step} {quota:?}: allowed 1ns before \
                     the advertised {wait}ns for {permits} permits"
                );
            }
            let delta = random_step(&mut rng, quota);
            limiter.time().advance(delta);
        }
    }
}

#[test]
fn token_bucket_retry_after_is_exact() {
    retry_after_is_exact::<TokenBucket<CellClock>>("bucket");
}

#[test]
fn gcra_retry_after_is_exact() {
    retry_after_is_exact::<Gcra<CellClock>>("gcra");
}

#[test]
fn sliding_window_retry_after_is_exact() {
    retry_after_is_exact::<SlidingWindow<CellClock>>("sliding");
}

#[test]
fn fixed_window_retry_after_is_exact() {
    retry_after_is_exact::<FixedWindow<CellClock>>("fixed");
}

// --- reference models ----------------------------------------------------

/// Token bucket, in `u128` credit-nanoseconds with no saturation tricks.
struct BucketModel {
    interval: u128,
    burst: u128,
    credit: u128,
    last: u128,
}

impl BucketModel {
    fn new(quota: Quota, now: u64) -> Self {
        let interval = quota.emission_interval().as_nanos();
        let burst = u128::from(quota.burst());
        Self {
            interval,
            burst,
            credit: interval * burst,
            last: u128::from(now),
        }
    }

    fn acquire(&mut self, now: u64, permits: u32) -> Decision {
        let now = u128::from(now);
        if now > self.last {
            self.credit = (self.credit + (now - self.last)).min(self.interval * self.burst);
            self.last = now;
        }
        let permits_u = u128::from(permits);
        if permits_u > self.burst {
            return Decision::Exceeds {
                capacity: u32::try_from(self.burst).expect("fits"),
            };
        }
        let cost = permits_u * self.interval;
        if cost <= self.credit {
            if permits > 0 {
                self.credit -= cost;
            }
            let remaining = (self.credit / self.interval).min(self.burst);
            Decision::Allow {
                remaining: u32::try_from(remaining).expect("fits"),
            }
        } else {
            Decision::Deny {
                retry_after: Duration::from_nanos(u64::try_from(cost - self.credit).expect("fits")),
            }
        }
    }
}

#[test]
fn token_bucket_and_gcra_match_the_reference_model() {
    for seed in 0..300_u64 {
        let mut rng = Lcg(seed ^ 0x0BAD_CAFE);
        let quota = random_quota(&mut rng);
        let start = rng.next() << 25;
        let mut model = BucketModel::new(quota, start);
        let mut bucket = TokenBucket::with_clock(quota, CellClock::at(start));
        let mut gcra = Gcra::with_clock(quota, CellClock::at(start));
        let mut now = start;
        for step in 0..400 {
            let permits = u32::try_from(rng.below(u64::from(quota.burst()) + 3)).expect("fits");
            let expected = model.acquire(now, permits);
            assert_eq!(
                bucket.try_acquire_n(permits),
                expected,
                "bucket seed {seed} step {step} {quota:?}"
            );
            assert_eq!(
                gcra.try_acquire_n(permits),
                expected,
                "gcra seed {seed} step {step} {quota:?}"
            );
            let delta = random_step(&mut rng, quota);
            now += delta;
            bucket.time().set(now);
            gcra.time().set(now);
        }
    }
}

/// Windows, recomputed from scratch from a log of grants on every call.
struct WindowModel {
    period: u128,
    limit: u128,
    sliding: bool,
    /// (window index, permits) for every grant ever made.
    log: Vec<(u128, u128)>,
}

impl WindowModel {
    fn used_in(&self, index: u128) -> u128 {
        self.log
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, n)| n)
            .sum()
    }

    fn estimate(&self, now: u128) -> u128 {
        let index = now / self.period;
        let current = self.used_in(index);
        if !self.sliding || index == 0 {
            return current;
        }
        let offset = now % self.period;
        let previous = self.used_in(index - 1);
        previous * (self.period - offset) / self.period + current
    }

    fn acquire(&mut self, now: u64, permits: u32) -> Option<bool> {
        let now = u128::from(now);
        let permits = u128::from(permits);
        if permits > self.limit {
            return None;
        }
        let fits = self.estimate(now) + permits <= self.limit;
        if fits && permits > 0 {
            self.log.push((now / self.period, permits));
        }
        Some(fits)
    }
}

fn windows_match_reference<L: Probe>(sliding: bool, name: &str) {
    for seed in 0..300_u64 {
        let mut rng = Lcg(seed ^ 0x51_1D_E5);
        let quota = random_quota(&mut rng);
        let start = rng.next() << 20;
        let mut model = WindowModel {
            period: quota.period().as_nanos(),
            limit: u128::from(quota.count()),
            sliding,
            log: Vec::new(),
        };
        let mut limiter = L::build(quota, CellClock::at(start));
        let mut now = start;
        for step in 0..400 {
            let permits = u32::try_from(rng.below(u64::from(quota.count()) + 3)).expect("fits");
            let got = limiter.try_acquire_n(permits);
            match model.acquire(now, permits) {
                None => assert!(
                    matches!(got, Decision::Exceeds { .. }),
                    "{name} seed {seed} step {step}: expected Exceeds, got {got:?}"
                ),
                Some(fits) => assert_eq!(
                    got.is_allowed(),
                    fits,
                    "{name} seed {seed} step {step} {quota:?}: {permits} permits at {now}, got {got:?}"
                ),
            }
            now += random_step(&mut rng, quota);
            limiter.time().set(now);
        }
    }
}

#[test]
fn fixed_window_matches_the_reference_model() {
    windows_match_reference::<FixedWindow<CellClock>>(false, "fixed");
}

#[test]
fn sliding_window_matches_the_reference_model() {
    windows_match_reference::<SlidingWindow<CellClock>>(true, "sliding");
}

// --- a clock that goes backwards never mints permits ---------------------

/// The `Clock` docs promise that a reading earlier than a previous one is
/// absorbed rather than turned into a wildly wrong permit count. The
/// strongest checkable form: whatever the clock does, no window (measured
/// on the *latest* reading seen) ever holds more than `count` grants for
/// the window limiters, and the bucket/GCRA never exceed
/// `burst + elapsed / interval`.
fn backwards_clock_never_mints<L: Probe>(windowed: bool, name: &str) {
    for seed in 0..200_u64 {
        let mut rng = Lcg(seed ^ 0xBAC4_3A2D);
        let quota = random_quota(&mut rng);
        let period = u64::try_from(quota.period().as_nanos()).expect("fits");
        let start = (rng.next() << 20) + 10 * period;
        let mut limiter = L::build(quota, CellClock::at(start));
        let mut high = start;
        let mut per_window: HashMap<u64, u64> = HashMap::new();
        let mut total = 0_u64;
        for step in 0..400 {
            // Mostly forward, sometimes back — by up to several periods.
            let reading = if rng.below(3) == 0 {
                high.saturating_sub(rng.below(3 * period + 1))
            } else {
                high + random_step(&mut rng, quota)
            };
            high = high.max(reading);
            limiter.time().set(reading);
            let permits = u32::try_from(1 + rng.below(u64::from(quota.count()))).expect("fits");
            if limiter.try_acquire_n(permits).is_allowed() {
                total += u64::from(permits);
                *per_window.entry(high / period).or_default() += u64::from(permits);
            }
            if windowed {
                for (window, used) in &per_window {
                    assert!(
                        *used <= u64::from(quota.count()),
                        "{name} seed {seed} step {step}: {used} grants in window {window} \
                         with a limit of {}",
                        quota.count()
                    );
                }
            } else {
                let interval = u64::try_from(quota.emission_interval().as_nanos()).expect("fits");
                let bound = u64::from(quota.burst()) + (high - start) / interval;
                assert!(
                    total <= bound,
                    "{name} seed {seed} step {step}: {total} grants, bound {bound}"
                );
            }
        }
    }
}

#[test]
fn token_bucket_survives_a_backwards_clock() {
    backwards_clock_never_mints::<TokenBucket<CellClock>>(false, "bucket");
}

#[test]
fn gcra_survives_a_backwards_clock() {
    backwards_clock_never_mints::<Gcra<CellClock>>(false, "gcra");
}

#[test]
fn fixed_window_survives_a_backwards_clock() {
    backwards_clock_never_mints::<FixedWindow<CellClock>>(true, "fixed");
}

#[test]
fn sliding_window_survives_a_backwards_clock() {
    backwards_clock_never_mints::<SlidingWindow<CellClock>>(true, "sliding");
}

// --- extreme rates and durations -----------------------------------------

/// Quotas at the edges of what `Quota` accepts, run on clocks that start
/// at zero and right up against `u64::MAX`. Tests run with overflow
/// checks on, so any unchecked arithmetic overflow is a panic here; the
/// assertions check that nothing is ever granted beyond the conservation
/// bound — saturation must fail closed, never open.
#[test]
fn extreme_quotas_and_clock_readings_never_overflow_or_fail_open() {
    let quotas = [
        Quota::new(1, Duration::from_nanos(1)).expect("1/ns"),
        Quota::new(u32::MAX, Duration::from_nanos(u64::from(u32::MAX))).expect("1/ns, huge"),
        Quota::new(u32::MAX, Duration::MAX).expect("max period"),
        Quota::new(1, Duration::MAX).expect("one per eternity"),
        Quota::new(1, Duration::from_secs(1))
            .and_then(|q| q.with_burst(u32::MAX))
            .expect("huge burst"),
        Quota::new(3, Duration::from_nanos(7))
            .and_then(|q| q.with_burst(u32::MAX))
            .expect("odd interval, huge burst"),
    ];
    let starts = [
        0,
        1,
        u64::MAX / 2,
        u64::MAX - 1_000_000_000_000,
        u64::MAX - 10,
        u64::MAX,
    ];
    let steps = [0, 1, 999, 1_000_000_000, u64::MAX / 4, u64::MAX];
    let permits = [0, 1, 2, 1_000, u32::MAX - 1, u32::MAX];

    fn exercise<L: Probe>(quota: Quota, start: u64, steps: &[u64], permits: &[u32], name: &str) {
        let mut limiter = L::build(quota, CellClock::at(start));
        let interval = quota.emission_interval().as_nanos();
        let mut granted: u128 = 0;
        for &step in steps {
            for &n in permits {
                for _ in 0..3 {
                    let decision = limiter.try_acquire_n(n);
                    let _ = limiter.peek_n(n);
                    let _ = limiter.available();
                    let _ = limiter.is_idle();
                    if decision.is_allowed() {
                        granted += u128::from(n);
                    }
                }
            }
            let elapsed = u128::from(limiter.time().now_nanos() - start);
            let bound = match name {
                // Each window index can hold `count`, and the run has
                // touched every index from the first reading's to the
                // latest's.
                "fixed" | "sliding" => {
                    let period = quota.period().as_nanos();
                    let now = u128::from(limiter.time().now_nanos());
                    let windows = now / period - u128::from(start) / period + 1;
                    u128::from(quota.count()) * windows
                }
                _ => u128::from(quota.burst()) + elapsed / interval,
            };
            assert!(
                granted <= bound,
                "{name} {quota:?} start {start}: granted {granted} > bound {bound}"
            );
            limiter.time().advance(step);
        }
        limiter.reset();
    }

    for quota in quotas {
        for start in starts {
            exercise::<TokenBucket<CellClock>>(quota, start, &steps, &permits, "bucket");
            exercise::<Gcra<CellClock>>(quota, start, &steps, &permits, "gcra");
            exercise::<SlidingWindow<CellClock>>(quota, start, &steps, &permits, "sliding");
            exercise::<FixedWindow<CellClock>>(quota, start, &steps, &permits, "fixed");
            let drained = Gcra::drained_with_clock(quota, CellClock::at(start));
            let _ = drained.next_permit_in();
        }
    }
}

// --- regressions found by the properties above ----------------------------

#[test]
fn gcra_at_the_end_of_the_clock_range_refuses_instead_of_admitting_everything() {
    // Before the fix the theoretical arrival time saturated at u64::MAX,
    // the same saturated TAT was stored on every call, and every request
    // was admitted. A schedule that cannot be represented now fails
    // closed (a documented limitation ~584 years after the clock's epoch).
    let quota = Quota::new(1, Duration::from_nanos(1)).expect("1/ns");
    let mut gcra = Gcra::with_clock(quota, CellClock::at(u64::MAX - 1));
    assert!(gcra.try_acquire().is_allowed());
    for _ in 0..10 {
        assert_eq!(
            gcra.try_acquire(),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }
    assert!(gcra.next_permit_in() > Duration::ZERO);
}

#[test]
fn window_limiters_do_not_reset_when_the_clock_steps_back_a_window() {
    // Before the fix any change of window index — including a backwards
    // one — cleared the counters and handed out a fresh budget.
    let quota = Quota::per_second(3).expect("valid");
    let second = 1_000_000_000;

    let mut fixed = FixedWindow::with_clock(quota, CellClock::at(5 * second));
    assert!(fixed.try_acquire_n(3).is_allowed());
    fixed.time().set(4 * second + 500_000_000);
    assert!(fixed.try_acquire().is_denied());
    assert_eq!(
        fixed.try_acquire().retry_after(),
        Some(Duration::from_millis(1_500)),
        "the wait runs to the end of the window the counter belongs to"
    );
    fixed.time().set(6 * second);
    assert_eq!(fixed.try_acquire_n(3), Decision::Allow { remaining: 0 });

    let mut sliding = SlidingWindow::with_clock(quota, CellClock::at(5 * second));
    assert!(sliding.try_acquire_n(3).is_allowed());
    sliding.time().set(4 * second);
    assert!(sliding.try_acquire().is_denied());
    sliding.time().set(7 * second);
    assert_eq!(sliding.try_acquire_n(3), Decision::Allow { remaining: 0 });
}

#[test]
fn sliding_window_retry_after_saturates_instead_of_wrapping() {
    // With a period of u64::MAX nanoseconds, "wait for the boundary, then
    // for this window's count to age out" exceeds u64. It used to wrap
    // (a 0ns wait in release builds, a panic in debug builds).
    let quota = Quota::new(2, Duration::MAX).expect("valid");
    let mut sliding = SlidingWindow::with_clock(quota, CellClock::at(0));
    assert!(sliding.try_acquire().is_allowed());
    assert_eq!(
        sliding.try_acquire_n(2),
        Decision::Deny {
            retry_after: Duration::from_nanos(u64::MAX)
        }
    );
}
