//! What the four algorithms do *differently*, pinned so that the
//! README's comparison table cannot drift away from the code.
//!
//! Every claim this crate makes about one algorithm being burstier,
//! smoother or cheaper than another is asserted here against the real
//! implementations, driven by the same manual clock.

use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use ratelimit::{
    Clock, Decision, FixedWindow, Gcra, ManualClock, Quota, RateLimiter, SlidingWindow, TokenBucket,
};

/// Ten permits per second, shared by every limiter under test.
fn quota() -> Quota {
    Quota::per_second(10).expect("valid quota")
}

/// A deterministic pseudo-random source. Not cryptography — the point is
/// a schedule that is irregular but reproducible on every machine, so a
/// failure can be re-run rather than guessed at.
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

// --- the token bucket and GCRA are the same algorithm --------------------

#[test]
fn bucket_and_gcra_agree_on_every_decision() {
    // The strongest form of the claim: over thousands of irregular
    // requests of varying sizes, at irregular intervals, the two
    // limiters return byte-identical decisions — including the exact
    // `retry_after` on every refusal.
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::with_clock(quota(), &clock);
    let mut gcra = Gcra::with_clock(quota(), &clock);
    let mut rng = Lcg(0x5EED);

    let mut allowed = 0;
    let mut denied = 0;
    for step in 0..20_000 {
        let permits = u32::try_from(rng.below(4)).expect("small");
        let from_bucket = bucket.try_acquire_n(permits);
        let from_gcra = gcra.try_acquire_n(permits);
        assert_eq!(from_bucket, from_gcra, "step {step}, {permits} permits");

        match from_bucket {
            Decision::Allow { .. } => allowed += 1,
            _ => denied += 1,
        }
        clock.advance(Duration::from_millis(rng.below(120)));
    }

    // Both counters are non-trivial, so the agreement above is not the
    // agreement of two limiters that never refused anything.
    assert!(allowed > 5_000, "only {allowed} allowed");
    assert!(denied > 5_000, "only {denied} denied");
}

#[test]
fn bucket_and_gcra_agree_on_peeks_and_availability() {
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::with_clock(quota(), &clock);
    let mut gcra = Gcra::with_clock(quota(), &clock);
    let mut rng = Lcg(0xC0FFEE);

    for step in 0..5_000 {
        let permits = u32::try_from(rng.below(12)).expect("small");
        assert_eq!(bucket.peek_n(permits), gcra.peek_n(permits), "peek {step}");
        assert_eq!(bucket.available(), gcra.available(), "available {step}");
        assert_eq!(bucket.is_idle(), gcra.is_idle(), "idle {step}");
        bucket.try_acquire_n(permits);
        gcra.try_acquire_n(permits);
        clock.advance(Duration::from_millis(rng.below(200)));
    }
}

#[test]
fn bucket_and_gcra_agree_at_every_burst_setting() {
    for burst in [1, 2, 3, 7, 10, 50, 1_000] {
        let quota = quota().with_burst(burst).expect("valid burst");
        let clock = ManualClock::new();
        let mut bucket = TokenBucket::with_clock(quota, &clock);
        let mut gcra = Gcra::with_clock(quota, &clock);
        let mut rng = Lcg(u64::from(burst) * 31 + 7);

        for step in 0..2_000 {
            let permits = u32::try_from(rng.below(u64::from(burst) + 2)).expect("small");
            assert_eq!(
                bucket.try_acquire_n(permits),
                gcra.try_acquire_n(permits),
                "burst {burst}, step {step}, {permits} permits"
            );
            clock.advance(Duration::from_millis(rng.below(150)));
        }
    }
}

#[test]
fn broken_clock_is_where_bucket_and_gcra_part_company() {
    // The equivalence above holds for any clock that keeps the [`Clock`]
    // contract. This is what happens when one does not: the bucket keeps
    // the credit it had banked, while GCRA refuses until the clock
    // catches up with the schedule it has already recorded. Neither
    // mints permits, which is the property that matters, but they are
    // not the same limiter any more.
    struct Stuck {
        readings: Vec<u64>,
        index: std::cell::Cell<usize>,
    }
    impl Clock for Stuck {
        fn now_nanos(&self) -> u64 {
            let i = self.index.get();
            self.index.set(i + 1);
            *self.readings.get(i).unwrap_or(&0)
        }
    }
    fn readings() -> Stuck {
        Stuck {
            readings: vec![0, 0, 1_000_000_000],
            index: std::cell::Cell::new(0),
        }
    }
    fn drain<L: RateLimiter>(limiter: &mut L) -> u32 {
        let mut taken = 0;
        while limiter.try_acquire().is_allowed() {
            taken += 1;
            assert!(taken < 1_000);
        }
        taken
    }

    let bucket_clock = readings();
    let gcra_clock = readings();
    let mut bucket = TokenBucket::with_clock(quota(), &bucket_clock);
    let mut gcra = Gcra::with_clock(quota(), &gcra_clock);

    assert_eq!(drain(&mut bucket), 11);
    assert_eq!(drain(&mut gcra), 2);
}

// --- where a burst lands decides everything ------------------------------

/// Records when a greedy client's permits were granted, in milliseconds.
fn grant_times<L: RateLimiter>(mut limiter: L, clock: &ManualClock, burst_at_ms: u64) -> Vec<u64> {
    let mut grants = Vec::new();
    clock.advance(Duration::from_millis(burst_at_ms));
    for ms in burst_at_ms..burst_at_ms + 3_000 {
        while limiter.try_acquire().is_allowed() {
            grants.push(ms);
        }
        clock.advance(Duration::from_millis(1));
    }
    grants
}

/// The most permits granted inside any one-second span.
///
/// Only spans that begin at a grant need checking: sliding a span right
/// until its left edge meets a grant can only increase what it contains.
fn busiest_second(grants: &[u64]) -> usize {
    grants
        .iter()
        .map(|&start| {
            grants
                .iter()
                .filter(|&&at| at >= start && at < start + 1_000)
                .count()
        })
        .max()
        .unwrap_or(0)
}

fn busiest_for_each(burst_at_ms: u64) -> (usize, usize, usize, usize) {
    let quota = quota();
    let bucket_clock = Arc::new(ManualClock::new());
    let gcra_clock = Arc::new(ManualClock::new());
    let sliding_clock = Arc::new(ManualClock::new());
    let fixed_clock = Arc::new(ManualClock::new());

    (
        busiest_second(&grant_times(
            TokenBucket::with_clock(quota, Arc::clone(&bucket_clock)),
            &bucket_clock,
            burst_at_ms,
        )),
        busiest_second(&grant_times(
            Gcra::with_clock(quota, Arc::clone(&gcra_clock)),
            &gcra_clock,
            burst_at_ms,
        )),
        busiest_second(&grant_times(
            SlidingWindow::with_clock(quota, Arc::clone(&sliding_clock)),
            &sliding_clock,
            burst_at_ms,
        )),
        busiest_second(&grant_times(
            FixedWindow::with_clock(quota, Arc::clone(&fixed_clock)),
            &fixed_clock,
            burst_at_ms,
        )),
    )
}

#[test]
fn burst_offset_sweep() {
    // A greedy client that starts asking at `offset` into a window, for
    // ten permits per second. The number is the most permits it ever got
    // inside a single second.
    //
    // This is the whole comparison in one table, and it is measured
    // rather than argued: the window limiters' behaviour depends on
    // *where the client happens to start*, and the bucket's does not.
    let mut report = String::from("offset bucket gcra sliding fixed\n");
    for offset in (0..1_000).step_by(100) {
        let (bucket, gcra, sliding, fixed) = busiest_for_each(offset);
        report.push_str(&format!(
            "{offset:>6} {bucket:>6} {gcra:>4} {sliding:>7} {fixed:>5}\n"
        ));
    }

    // The bucket and GCRA absorb the whole burst instantly and then trickle
    // in one more permit per 100ms; where the client starts makes no
    // difference to how many land inside a one-second span. The sliding
    // window's count grows with the offset because more of the *previous*
    // window's ten permits are still weighted in as the offset approaches a
    // full window. The fixed window is the outlier at offset 0, because a
    // client that starts exactly on a boundary sees only that window's ten
    // -- any other offset straddles a boundary and doubles up.
    let mut expected = String::from("offset bucket gcra sliding fixed\n");
    for (offset, bucket, gcra, sliding, fixed) in [
        (0u64, 19u64, 19u64, 10u64, 10u64),
        (100, 19, 19, 11, 20),
        (200, 19, 19, 12, 20),
        (300, 19, 19, 13, 20),
        (400, 19, 19, 14, 20),
        (500, 19, 19, 15, 20),
        (600, 19, 19, 16, 20),
        (700, 19, 19, 17, 20),
        (800, 19, 19, 18, 20),
        (900, 19, 19, 19, 20),
    ] {
        expected.push_str(&format!(
            "{offset:>6} {bucket:>6} {gcra:>4} {sliding:>7} {fixed:>5}\n"
        ));
    }

    assert_eq!(report, expected);
}

#[test]
fn only_a_bucket_can_be_configured_below_the_window_limit() {
    // The window limiters' burst allowance is their budget and cannot be
    // reduced; a bucket's is a separate dial. This is the practical
    // reason to reach for a bucket when a burst is genuinely unwanted.
    let quota = quota().with_burst(1).expect("valid burst");
    let clock = Arc::new(ManualClock::new());
    let paced = TokenBucket::with_clock(quota, Arc::clone(&clock));
    let grants = grant_times(paced, &clock, 500);
    assert_eq!(
        busiest_second(&grants),
        10,
        "one permit per interval, half-open window"
    );
}

// --- recovery shapes -----------------------------------------------------

#[test]
fn recovery_shapes_differ() {
    // Permits available at each tenth of a second after all four have
    // been drained at the same instant. Three distinct shapes: a drip, a
    // slope that starts one window later, and a cliff.
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::with_clock(quota(), &clock);
    let mut gcra = Gcra::with_clock(quota(), &clock);
    let mut sliding = SlidingWindow::with_clock(quota(), &clock);
    let mut fixed = FixedWindow::with_clock(quota(), &clock);

    for _ in 0..10 {
        bucket.try_acquire();
        gcra.try_acquire();
        sliding.try_acquire();
        fixed.try_acquire();
    }

    let mut shapes: [Vec<u32>; 4] = Default::default();
    for step in 0..=20 {
        if step > 0 {
            clock.advance(Duration::from_millis(100));
        }
        shapes[0].push(bucket.available());
        shapes[1].push(gcra.available());
        shapes[2].push(sliding.available());
        shapes[3].push(fixed.available());
    }

    let drip: Vec<u32> = vec![
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
    ];
    let slope: Vec<u32> = vec![
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
    ];
    let cliff: Vec<u32> = vec![
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
    ];

    assert_eq!(shapes[0], drip, "token bucket");
    assert_eq!(shapes[1], drip, "gcra");
    assert_eq!(shapes[2], slope, "sliding window");
    assert_eq!(shapes[3], cliff, "fixed window");
}

#[test]
fn traffic_at_exactly_the_permitted_rate() {
    // One request every 100 ms for ten seconds, against a 10/s quota.
    // Only the sliding window refuses any of it, and this is the exact
    // cost of its approximation.
    fn allowed<L: RateLimiter>(mut limiter: L, clock: &ManualClock) -> usize {
        let mut count = 0;
        for _ in 0..100 {
            if limiter.try_acquire().is_allowed() {
                count += 1;
            }
            clock.advance(Duration::from_millis(100));
        }
        count
    }

    let clock = ManualClock::new();
    assert_eq!(
        allowed(TokenBucket::with_clock(quota(), &clock), &clock),
        100
    );
    let clock = ManualClock::new();
    assert_eq!(allowed(Gcra::with_clock(quota(), &clock), &clock), 100);
    let clock = ManualClock::new();
    assert_eq!(
        allowed(SlidingWindow::with_clock(quota(), &clock), &clock),
        95
    );
    let clock = ManualClock::new();
    assert_eq!(
        allowed(FixedWindow::with_clock(quota(), &clock), &clock),
        100
    );
}

#[test]
fn all_four_hold_the_same_long_run_rate() {
    // Sixty seconds of a client asking as fast as it can. The long-run
    // rate is the thing they all agree on; everything else in this file
    // is about the short run.
    fn allowed<L: RateLimiter>(mut limiter: L, clock: &ManualClock) -> usize {
        let mut count = 0;
        for _ in 0..60_000 {
            while limiter.try_acquire().is_allowed() {
                count += 1;
            }
            clock.advance(Duration::from_millis(1));
        }
        count
    }

    let clock = ManualClock::new();
    let bucket = allowed(TokenBucket::with_clock(quota(), &clock), &clock);
    let clock = ManualClock::new();
    let gcra = allowed(Gcra::with_clock(quota(), &clock), &clock);
    let clock = ManualClock::new();
    let sliding = allowed(SlidingWindow::with_clock(quota(), &clock), &clock);
    let clock = ManualClock::new();
    let fixed = allowed(FixedWindow::with_clock(quota(), &clock), &clock);

    // The loop's last check lands at 59.999s, not 60s (each iteration
    // checks, then advances the clock by 1ms), so the bucket and GCRA
    // bank floor(59.999 * 10) = 599 refilled permits on top of their
    // 10-permit opening burst: 609, not 610. The windows do not add an
    // opening burst at all, because it is part of the first window's
    // budget, so a whole 60 windows of 10 gives exactly 600 either way.
    assert_eq!((bucket, gcra, sliding, fixed), (609, 609, 600, 600));
}

// --- the shape of a refusal ----------------------------------------------

#[test]
fn what_counts_as_impossible_differs() {
    // A bucket's ceiling is its burst; a window's is its budget. Same
    // quota, different answers to "could this request ever succeed".
    let quota = quota().with_burst(3).expect("valid burst");
    let clock = ManualClock::new();

    assert_eq!(
        TokenBucket::with_clock(quota, &clock).try_acquire_n(5),
        Decision::Exceeds { capacity: 3 }
    );
    assert_eq!(
        Gcra::with_clock(quota, &clock).try_acquire_n(5),
        Decision::Exceeds { capacity: 3 }
    );
    // The windows ignore burst, so five permits are merely unavailable
    // right now, not impossible.
    assert_eq!(
        SlidingWindow::with_clock(quota, &clock).try_acquire_n(5),
        Decision::Allow { remaining: 5 }
    );
    assert_eq!(
        FixedWindow::with_clock(quota, &clock).try_acquire_n(5),
        Decision::Allow { remaining: 5 }
    );
}

#[test]
fn all_four_refuse_everything_while_the_clock_stands_still() {
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::with_clock(quota(), &clock);
    let mut gcra = Gcra::with_clock(quota(), &clock);
    let mut sliding = SlidingWindow::with_clock(quota(), &clock);
    let mut fixed = FixedWindow::with_clock(quota(), &clock);

    for _ in 0..10 {
        bucket.try_acquire();
        gcra.try_acquire();
        sliding.try_acquire();
        fixed.try_acquire();
    }
    for _ in 0..100 {
        assert!(bucket.try_acquire().is_denied());
        assert!(gcra.try_acquire().is_denied());
        assert!(sliding.try_acquire().is_denied());
        assert!(fixed.try_acquire().is_denied());
    }
}

#[test]
fn every_denial_advertises_a_wait_that_works() {
    // Across all four: wait exactly as long as you were told, and the
    // request succeeds. This is the contract that makes `retry_after`
    // worth putting in a `Retry-After` header.
    fn check<L: RateLimiter>(mut limiter: L, clock: &ManualClock, name: &str) {
        for _ in 0..20 {
            while limiter.try_acquire().is_allowed() {}
            let retry = limiter
                .try_acquire()
                .retry_after()
                .unwrap_or_else(|| panic!("{name} refused without a wait"));
            assert!(retry > Duration::ZERO, "{name} advertised a zero wait");
            clock.advance(retry);
            assert!(limiter.try_acquire().is_allowed(), "{name} after its wait");
        }
    }

    let clock = ManualClock::new();
    check(TokenBucket::with_clock(quota(), &clock), &clock, "bucket");
    let clock = ManualClock::new();
    check(Gcra::with_clock(quota(), &clock), &clock, "gcra");
    let clock = ManualClock::new();
    check(
        SlidingWindow::with_clock(quota(), &clock),
        &clock,
        "sliding",
    );
    let clock = ManualClock::new();
    check(FixedWindow::with_clock(quota(), &clock), &clock, "fixed");
}

// --- cost ----------------------------------------------------------------

#[test]
fn per_limiter_state_is_what_the_documentation_claims() {
    // Measured against a zero-sized clock, so the numbers are the
    // limiter's own state and not the clock's. The quota is subtracted
    // because it is configuration, shared by every limiter enforcing the
    // same rule, rather than per-key state.
    struct Frozen;
    impl Clock for Frozen {
        fn now_nanos(&self) -> u64 {
            0
        }
    }

    let quota = size_of::<Quota>();
    assert_eq!(size_of::<TokenBucket<Frozen>>() - quota, 16, "two u64");
    assert_eq!(size_of::<Gcra<Frozen>>() - quota, 8, "one u64");
    assert_eq!(
        size_of::<SlidingWindow<Frozen>>() - quota,
        16,
        "u64 + 2 u32"
    );
    assert_eq!(size_of::<FixedWindow<Frozen>>() - quota, 16, "u64 + u32");
}

#[test]
fn one_quota_configures_all_four() {
    // The API-level claim: choosing an algorithm never means rewriting
    // the configuration.
    let quota = quota();
    let clock = ManualClock::new();

    assert_eq!(TokenBucket::with_clock(quota, &clock).quota(), quota);
    assert_eq!(Gcra::with_clock(quota, &clock).quota(), quota);
    assert_eq!(SlidingWindow::with_clock(quota, &clock).quota(), quota);
    assert_eq!(FixedWindow::with_clock(quota, &clock).quota(), quota);
}
