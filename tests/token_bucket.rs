//! Token bucket behaviour, asserted against a manual clock.
//!
//! Nothing here sleeps. Every assertion about timing is exact, including
//! the ones that stand one nanosecond either side of a threshold, which
//! is the whole reason the clock is injectable.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ratelimit::{Clock, Decision, ManualClock, Quota, RateLimiter, Shared, TokenBucket};

/// Ten permits per second: a 100 ms emission interval, which makes every
/// expected duration in this file a round number.
fn quota() -> Quota {
    Quota::per_second(10).expect("valid quota")
}

fn bucket(clock: &ManualClock) -> TokenBucket<&ManualClock> {
    TokenBucket::with_clock(quota(), clock)
}

/// Drains the bucket, returning how many permits it gave up.
fn drain<L: RateLimiter>(limiter: &mut L) -> u32 {
    let mut taken = 0;
    while limiter.try_acquire().is_allowed() {
        taken += 1;
        assert!(taken < 10_000, "bucket never ran dry");
    }
    taken
}

// --- initial state -------------------------------------------------------

#[test]
fn starts_full() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(bucket.available(), 10);
}

#[test]
fn starts_idle() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert!(bucket.is_idle());
}

#[test]
fn empty_bucket_starts_drained() {
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::empty_with_clock(quota(), &clock);
    assert_eq!(bucket.available(), 0);
    assert!(!bucket.is_idle());
    assert_eq!(
        bucket.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_millis(100)
        }
    );
}

#[test]
fn empty_bucket_fills_at_the_quota_rate() {
    let clock = ManualClock::new();
    let mut bucket = TokenBucket::empty_with_clock(quota(), &clock);
    clock.advance(Duration::from_millis(500));
    assert_eq!(bucket.available(), 5);
}

#[test]
fn quota_is_reported_back() {
    let clock = ManualClock::new();
    let bucket = bucket(&clock);
    assert_eq!(bucket.quota(), quota());
}

#[test]
fn epoch_offset_does_not_change_behaviour() {
    // The limiter must depend only on differences between readings. A
    // clock that starts at an arbitrary point is the cheapest way to
    // catch an implementation that accidentally treats zero as special.
    let clock = ManualClock::starting_at(1_234_567_890_123);
    let mut bucket = bucket(&clock);
    assert_eq!(drain(&mut bucket), 10);
    clock.advance(Duration::from_millis(100));
    assert!(bucket.try_acquire().is_allowed());
}

// --- burst absorption ----------------------------------------------------

#[test]
fn whole_burst_is_absorbed_at_one_instant() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(drain(&mut bucket), 10);
    assert_eq!(clock.now_nanos(), 0, "no time passed during the burst");
}

#[test]
fn burst_can_be_taken_in_one_call() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(bucket.try_acquire_n(10), Decision::Allow { remaining: 0 });
}

#[test]
fn burst_is_configurable_above_the_rate() {
    let clock = ManualClock::new();
    let quota = quota().with_burst(25).expect("valid burst");
    let mut bucket = TokenBucket::with_clock(quota, &clock);
    assert_eq!(drain(&mut bucket), 25);
}

#[test]
fn burst_of_one_is_a_strict_pacer() {
    let clock = ManualClock::new();
    let quota = quota().with_burst(1).expect("valid burst");
    let mut bucket = TokenBucket::with_clock(quota, &clock);

    assert!(bucket.try_acquire().is_allowed());
    assert!(bucket.try_acquire().is_denied());

    // Ten intervals of silence still only buys one permit: the bucket
    // cannot hold more than its burst.
    clock.advance(Duration::from_secs(1));
    assert!(bucket.try_acquire().is_allowed());
    assert!(bucket.try_acquire().is_denied());
}

#[test]
fn remaining_counts_down_through_the_burst() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    for expected in (0..10).rev() {
        assert_eq!(
            bucket.try_acquire(),
            Decision::Allow {
                remaining: expected
            }
        );
    }
}

#[test]
fn exhausted_bucket_denies() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    assert!(bucket.try_acquire().is_denied());
}

// --- recovery ------------------------------------------------------------

#[test]
fn deny_reports_exactly_one_interval() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    assert_eq!(
        bucket.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_millis(100)
        }
    );
}

#[test]
fn deny_for_several_permits_reports_the_full_wait() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    assert_eq!(
        bucket.try_acquire_n(4),
        Decision::Deny {
            retry_after: Duration::from_millis(400)
        }
    );
}

#[test]
fn waiting_exactly_the_advertised_time_succeeds() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);

    let retry = bucket
        .try_acquire_n(3)
        .retry_after()
        .expect("a denial carries a wait");
    clock.advance(retry);
    assert!(bucket.try_acquire_n(3).is_allowed());
}

#[test]
fn one_nanosecond_short_of_the_advertised_time_still_fails() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);

    let retry = bucket.try_acquire().retry_after().expect("a wait");
    clock.advance(retry - Duration::from_nanos(1));
    assert!(bucket.try_acquire().is_denied());
    clock.advance(Duration::from_nanos(1));
    assert!(bucket.try_acquire().is_allowed());
}

#[test]
fn recovery_is_a_drip_of_one_permit_per_interval() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);

    for expected in 1..=10 {
        clock.advance(Duration::from_millis(100));
        assert_eq!(bucket.available(), expected, "after {expected} intervals");
    }
}

#[test]
fn refill_is_continuous_not_quantised() {
    // A timer-driven implementation would grant nothing until its next
    // tick. Credit here accrues by the nanosecond, so a permit is
    // available the instant it has been earned, whenever that falls.
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);

    clock.advance(Duration::from_millis(150));
    assert_eq!(bucket.available(), 1);
    clock.advance(Duration::from_millis(50));
    assert_eq!(bucket.available(), 2);
}

#[test]
fn partial_interval_grants_nothing() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    clock.advance(Duration::from_millis(99));
    assert_eq!(bucket.available(), 0);
    assert!(bucket.try_acquire().is_denied());
}

#[test]
fn credit_stops_at_the_burst_allowance() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);

    // An hour of silence is worth exactly one full bucket, not an hour's
    // worth of permits.
    clock.advance(Duration::from_secs(3600));
    assert_eq!(bucket.available(), 10);
    assert_eq!(drain(&mut bucket), 10);
}

#[test]
fn is_idle_only_once_fully_refilled() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    bucket.try_acquire();
    assert!(!bucket.is_idle());

    clock.advance(Duration::from_millis(100) - Duration::from_nanos(1));
    assert!(!bucket.is_idle(), "one nanosecond short of full");
    clock.advance(Duration::from_nanos(1));
    assert!(bucket.is_idle());
}

#[test]
fn reset_restores_a_full_bucket() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    bucket.reset();
    assert_eq!(bucket.available(), 10);
}

// --- steady state --------------------------------------------------------

#[test]
fn traffic_at_exactly_the_rate_is_never_refused() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    for i in 0..1_000 {
        assert!(bucket.try_acquire().is_allowed(), "request {i}");
        clock.advance(Duration::from_millis(100));
    }
}

#[test]
fn traffic_just_under_the_rate_is_never_refused() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    for i in 0..1_000 {
        assert!(bucket.try_acquire().is_allowed(), "request {i}");
        clock.advance(Duration::from_millis(101));
    }
}

#[test]
fn traffic_above_the_rate_settles_to_the_rate() {
    // Hammering as fast as possible for ten seconds may not beat the
    // sustained rate by more than the one-off burst.
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);

    let mut allowed = 0;
    for step in 0..=10_000 {
        if bucket.try_acquire().is_allowed() {
            allowed += 1;
        }
        if step < 10_000 {
            clock.advance(Duration::from_millis(1));
        }
    }

    // 10 s at 10/s, plus the burst that was banked before the run.
    assert_eq!(allowed, 100 + 10);
}

#[test]
fn permits_over_a_span_never_exceed_burst_plus_rate() {
    // The bound stated in the type documentation, checked at several
    // spans: over any interval `d`, at most `burst + d / interval`.
    let interval = Duration::from_millis(100);
    for span_ms in [0_u64, 1, 99, 100, 250, 1_000, 5_000] {
        let clock = ManualClock::new();
        let mut bucket = bucket(&clock);

        let mut allowed = 0;
        for step in 0..=span_ms {
            while bucket.try_acquire().is_allowed() {
                allowed += 1;
            }
            if step < span_ms {
                clock.advance(Duration::from_millis(1));
            }
        }

        let ceiling = 10 + (span_ms * 1_000_000) / u64::try_from(interval.as_nanos()).unwrap();
        assert!(
            allowed <= ceiling,
            "{allowed} permits over {span_ms} ms exceeds the bound of {ceiling}"
        );
    }
}

// --- all-or-nothing, and requests that never fit -------------------------

#[test]
fn a_refused_request_consumes_nothing() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    bucket.try_acquire_n(8);
    assert_eq!(bucket.available(), 2);

    assert!(bucket.try_acquire_n(5).is_denied());
    assert_eq!(bucket.available(), 2, "the refusal took nothing");
}

#[test]
fn a_large_request_does_not_starve_a_small_one() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    bucket.try_acquire_n(9);
    assert!(bucket.try_acquire_n(5).is_denied());
    assert!(bucket.try_acquire().is_allowed());
}

#[test]
fn asking_for_more_than_the_burst_can_never_succeed() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(bucket.try_acquire_n(11), Decision::Exceeds { capacity: 10 });

    // Even from a full bucket after unlimited waiting.
    clock.advance(Duration::from_secs(3600));
    assert_eq!(bucket.try_acquire_n(11), Decision::Exceeds { capacity: 10 });
}

#[test]
fn exceeds_offers_no_retry() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(bucket.try_acquire_n(u32::MAX).retry_after(), None);
}

#[test]
fn exceeds_consumes_nothing() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    bucket.try_acquire_n(11);
    assert_eq!(bucket.available(), 10);
}

#[test]
fn zero_permits_are_always_allowed() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    assert_eq!(bucket.try_acquire_n(0), Decision::Allow { remaining: 0 });
}

#[test]
fn zero_permits_consume_nothing() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    assert_eq!(bucket.try_acquire_n(0), Decision::Allow { remaining: 10 });
    assert_eq!(bucket.available(), 10);
}

// --- peeking -------------------------------------------------------------

#[test]
fn peek_does_not_consume() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    for _ in 0..100 {
        assert!(bucket.peek().is_allowed());
    }
    assert_eq!(bucket.available(), 10);
}

#[test]
fn peek_matches_the_decision_that_follows_it() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    for _ in 0..15 {
        let peeked = bucket.peek_n(2);
        let taken = bucket.try_acquire_n(2);
        assert_eq!(peeked, taken);
        clock.advance(Duration::from_millis(30));
    }
}

#[test]
fn peek_sees_time_pass() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    assert!(bucket.peek().is_denied());
    clock.advance(Duration::from_millis(100));
    assert!(bucket.peek().is_allowed());
}

// --- clock misbehaviour --------------------------------------------------

#[test]
fn a_clock_that_stands_still_grants_nothing_extra() {
    let clock = ManualClock::new();
    let mut bucket = bucket(&clock);
    drain(&mut bucket);
    for _ in 0..1_000 {
        assert!(bucket.try_acquire().is_denied());
    }
}

#[test]
fn a_backwards_clock_cannot_mint_permits() {
    // `ManualClock` refuses to go backwards, so this drives the limiter
    // from a deliberately broken clock instead. The limiter must treat a
    // backwards step as no time passing at all.
    struct Backwards {
        readings: Vec<u64>,
        index: std::cell::Cell<usize>,
    }
    impl Clock for Backwards {
        fn now_nanos(&self) -> u64 {
            let i = self.index.get();
            self.index.set(i + 1);
            *self.readings.get(i).unwrap_or(&0)
        }
    }

    let clock = Backwards {
        // The constructor takes the first reading. Then: one permit at
        // t=0, a step forward to t=1 s, and from there a clock stuck in
        // the past.
        readings: vec![0, 0, 1_000_000_000],
        index: std::cell::Cell::new(0),
    };
    let mut bucket = TokenBucket::with_clock(quota(), &clock);

    // The step to t=1 s refills the one spent permit (and no more: the
    // bucket caps). Every reading after that is in the past, and credits
    // nothing at all. Eleven, and never twelve.
    assert_eq!(drain(&mut bucket), 11);
}

// --- concurrency ---------------------------------------------------------

#[test]
fn concurrent_callers_share_one_budget_exactly() {
    // The clock is frozen, so the answer is not a range: eight threads
    // asking 50 times each against a budget of 100 must see exactly 100
    // permits granted, however they interleave.
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(TokenBucket::with_clock(quota, clock)));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let limiter = Arc::clone(&limiter);
        handles.push(thread::spawn(move || {
            (0..50)
                .filter(|_| limiter.try_acquire().is_allowed())
                .count()
        }));
    }

    let allowed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(allowed, 100);
    assert_eq!(limiter.available(), 0);
}

#[test]
fn concurrent_callers_share_a_moving_budget_exactly() {
    // Time is advanced between two rounds rather than inside them, which
    // keeps the expected total exact. Advancing the clock *during* a
    // round would not: whether the last permit earned gets claimed
    // before the threads run out of attempts depends on the scheduler,
    // and a test whose expected value depends on the scheduler is the
    // flaky test this crate exists to avoid.
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(TokenBucket::with_clock(
        quota,
        Arc::clone(&clock),
    )));

    let round = || {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let limiter = Arc::clone(&limiter);
            handles.push(thread::spawn(move || {
                (0..100)
                    .filter(|_| limiter.try_acquire().is_allowed())
                    .count()
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum::<usize>()
    };

    assert_eq!(round(), 100, "the whole bucket, and no more");

    clock.advance(Duration::from_millis(100));
    assert_eq!(round(), 10, "exactly 100 ms of refill");
}

#[test]
fn concurrent_multi_permit_requests_stay_all_or_nothing() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(TokenBucket::with_clock(quota, clock)));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let limiter = Arc::clone(&limiter);
        handles.push(thread::spawn(move || {
            (0..20)
                .filter(|_| limiter.try_acquire_n(3).is_allowed())
                .count()
        }));
    }

    let granted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    // 100 permits in threes: 33 whole requests, and one permit stranded.
    assert_eq!(granted, 33);
    assert_eq!(limiter.available(), 1);
}
