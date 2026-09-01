//! GCRA behaviour, asserted against a manual clock.
//!
//! The tests that pin GCRA *against the token bucket* live in
//! `tests/comparison.rs`; these pin it on its own terms — the theoretical
//! arrival time, the tolerance, and the scheduling hint.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ratelimit::{Clock, Decision, Gcra, ManualClock, Quota, RateLimiter, Shared};

/// Ten permits per second: a 100 ms emission interval.
fn quota() -> Quota {
    Quota::per_second(10).expect("valid quota")
}

fn gcra(clock: &ManualClock) -> Gcra<&ManualClock> {
    Gcra::with_clock(quota(), clock)
}

fn drain<L: RateLimiter>(limiter: &mut L) -> u32 {
    let mut taken = 0;
    while limiter.try_acquire().is_allowed() {
        taken += 1;
        assert!(taken < 10_000, "limiter never ran dry");
    }
    taken
}

// --- initial state -------------------------------------------------------

#[test]
fn starts_with_the_full_tolerance() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    assert_eq!(gcra.available(), 10);
    assert!(gcra.is_idle());
}

#[test]
fn drained_limiter_starts_with_none() {
    let clock = ManualClock::new();
    let mut gcra = Gcra::drained_with_clock(quota(), &clock);
    assert_eq!(gcra.available(), 0);
    assert!(!gcra.is_idle());
    assert_eq!(
        gcra.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_millis(100)
        }
    );
}

#[test]
fn drained_limiter_recovers_at_the_quota_rate() {
    let clock = ManualClock::new();
    let mut gcra = Gcra::drained_with_clock(quota(), &clock);
    clock.advance(Duration::from_millis(500));
    assert_eq!(gcra.available(), 5);
}

#[test]
fn quota_is_reported_back() {
    let clock = ManualClock::new();
    assert_eq!(gcra(&clock).quota(), quota());
}

#[test]
fn epoch_offset_does_not_change_behaviour() {
    let clock = ManualClock::starting_at(987_654_321_000_000);
    let mut gcra = gcra(&clock);
    assert_eq!(drain(&mut gcra), 10);
    clock.advance(Duration::from_millis(100));
    assert!(gcra.try_acquire().is_allowed());
}

// --- the theoretical arrival time ----------------------------------------

#[test]
fn tat_starts_at_now() {
    let clock = ManualClock::starting_at(42);
    let gcra = gcra(&clock);
    assert_eq!(gcra.tat_nanos(), 42);
}

#[test]
fn tat_advances_by_one_interval_per_permit() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for taken in 1..=10 {
        gcra.try_acquire();
        assert_eq!(gcra.tat_nanos(), taken * 100_000_000);
    }
}

#[test]
fn tat_advances_by_the_whole_batch() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    gcra.try_acquire_n(4);
    assert_eq!(gcra.tat_nanos(), 400_000_000);
}

#[test]
fn tat_does_not_move_on_a_denial() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    let tat = gcra.tat_nanos();
    assert!(gcra.try_acquire().is_denied());
    assert_eq!(gcra.tat_nanos(), tat, "a refusal schedules nothing");
}

#[test]
fn tat_does_not_move_on_a_peek() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    gcra.peek_n(5);
    assert_eq!(gcra.tat_nanos(), 0);
}

#[test]
fn tat_in_the_past_means_idle() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    gcra.try_acquire();
    assert!(gcra.tat_nanos() > clock.now_nanos());
    assert!(!gcra.is_idle());

    clock.advance(Duration::from_millis(100));
    assert_eq!(gcra.tat_nanos(), clock.now_nanos());
    assert!(gcra.is_idle());
}

// --- the scheduling hint -------------------------------------------------

#[test]
fn next_permit_is_immediate_when_one_is_available() {
    let clock = ManualClock::new();
    let gcra = gcra(&clock);
    assert_eq!(gcra.next_permit_in(), Duration::ZERO);
}

#[test]
fn next_permit_counts_down_in_real_time() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    assert_eq!(gcra.next_permit_in(), Duration::from_millis(100));

    clock.advance(Duration::from_millis(60));
    assert_eq!(gcra.next_permit_in(), Duration::from_millis(40));

    clock.advance(Duration::from_millis(40));
    assert_eq!(gcra.next_permit_in(), Duration::ZERO);
}

#[test]
fn sleeping_for_the_hint_always_wins_the_permit() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for _ in 0..50 {
        drain(&mut gcra);
        clock.advance(gcra.next_permit_in());
        assert!(gcra.try_acquire().is_allowed());
    }
}

#[test]
fn the_hint_agrees_with_the_denial() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    let hint = gcra.next_permit_in();
    assert_eq!(gcra.try_acquire().retry_after(), Some(hint));
}

// --- burst tolerance -----------------------------------------------------

#[test]
fn whole_tolerance_is_available_at_one_instant() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    assert_eq!(drain(&mut gcra), 10);
    assert_eq!(clock.now_nanos(), 0);
}

#[test]
fn tolerance_of_one_never_allows_two_at_once() {
    let clock = ManualClock::new();
    let quota = quota().with_burst(1).expect("valid burst");
    let mut gcra = Gcra::with_clock(quota, &clock);

    for _ in 0..20 {
        assert!(gcra.try_acquire().is_allowed());
        assert!(gcra.try_acquire().is_denied(), "never two at one instant");
        clock.advance(Duration::from_millis(100));
    }
}

#[test]
fn idling_does_not_bank_more_than_the_tolerance() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    clock.advance(Duration::from_secs(86_400));
    assert_eq!(drain(&mut gcra), 10, "a day of silence buys one tolerance");
}

#[test]
fn tolerance_above_the_rate_is_honoured() {
    let clock = ManualClock::new();
    let quota = quota().with_burst(30).expect("valid burst");
    let mut gcra = Gcra::with_clock(quota, &clock);
    assert_eq!(drain(&mut gcra), 30);
}

#[test]
fn remaining_counts_down_through_the_tolerance() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for expected in (0..10).rev() {
        assert_eq!(
            gcra.try_acquire(),
            Decision::Allow {
                remaining: expected
            }
        );
    }
}

// --- recovery ------------------------------------------------------------

#[test]
fn deny_reports_exactly_one_interval() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    assert_eq!(
        gcra.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_millis(100)
        }
    );
}

#[test]
fn deny_for_a_batch_reports_the_whole_wait() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    gcra.try_acquire_n(8);
    // Two permits of slack left, so a batch of five needs three more
    // intervals of schedule.
    assert_eq!(
        gcra.try_acquire_n(5),
        Decision::Deny {
            retry_after: Duration::from_millis(300)
        }
    );
}

#[test]
fn waiting_exactly_the_advertised_time_succeeds() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    let retry = gcra.try_acquire_n(6).retry_after().expect("a wait");
    clock.advance(retry);
    assert!(gcra.try_acquire_n(6).is_allowed());
}

#[test]
fn one_nanosecond_short_still_fails() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    clock.advance(Duration::from_millis(100) - Duration::from_nanos(1));
    assert!(gcra.try_acquire().is_denied());
    clock.advance(Duration::from_nanos(1));
    assert!(gcra.try_acquire().is_allowed());
}

#[test]
fn recovery_is_a_drip_of_one_permit_per_interval() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    for expected in 1..=10 {
        clock.advance(Duration::from_millis(100));
        assert_eq!(gcra.available(), expected);
    }
}

#[test]
fn reset_restores_the_full_tolerance() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    gcra.reset();
    assert_eq!(gcra.available(), 10);
    assert_eq!(gcra.tat_nanos(), clock.now_nanos());
}

// --- steady state --------------------------------------------------------

#[test]
fn traffic_at_exactly_the_rate_is_never_refused() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for i in 0..1_000 {
        assert!(gcra.try_acquire().is_allowed(), "request {i}");
        clock.advance(Duration::from_millis(100));
    }
}

#[test]
fn traffic_above_the_rate_settles_to_the_rate() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    let mut allowed = 0;
    for step in 0..=10_000 {
        if gcra.try_acquire().is_allowed() {
            allowed += 1;
        }
        if step < 10_000 {
            clock.advance(Duration::from_millis(1));
        }
    }
    assert_eq!(allowed, 100 + 10);
}

// --- all-or-nothing, and requests that never fit -------------------------

#[test]
fn a_refused_batch_consumes_nothing() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    gcra.try_acquire_n(8);
    assert!(gcra.try_acquire_n(5).is_denied());
    assert_eq!(gcra.available(), 2);
    assert_eq!(gcra.tat_nanos(), 800_000_000);
}

#[test]
fn asking_for_more_than_the_tolerance_can_never_succeed() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    assert_eq!(gcra.try_acquire_n(11), Decision::Exceeds { capacity: 10 });
    clock.advance(Duration::from_secs(3600));
    assert_eq!(gcra.try_acquire_n(11), Decision::Exceeds { capacity: 10 });
}

#[test]
fn exceeds_offers_no_retry_and_takes_nothing() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    assert_eq!(gcra.try_acquire_n(u32::MAX).retry_after(), None);
    assert_eq!(gcra.available(), 10);
}

#[test]
fn zero_permits_are_always_allowed() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    assert_eq!(gcra.try_acquire_n(0), Decision::Allow { remaining: 0 });
    assert_eq!(gcra.tat_nanos(), 1_000_000_000);
}

// --- peeking -------------------------------------------------------------

#[test]
fn peek_does_not_consume() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for _ in 0..100 {
        assert!(gcra.peek().is_allowed());
    }
    assert_eq!(gcra.available(), 10);
}

#[test]
fn peek_matches_the_decision_that_follows_it() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    for _ in 0..15 {
        let peeked = gcra.peek_n(2);
        assert_eq!(peeked, gcra.try_acquire_n(2));
        clock.advance(Duration::from_millis(30));
    }
}

// --- clock misbehaviour --------------------------------------------------

#[test]
fn a_clock_that_stands_still_grants_nothing_extra() {
    let clock = ManualClock::new();
    let mut gcra = gcra(&clock);
    drain(&mut gcra);
    for _ in 0..1_000 {
        assert!(gcra.try_acquire().is_denied());
    }
}

#[test]
fn a_backwards_clock_cannot_mint_permits() {
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
        // Construct at 0, one permit at 0, a step to 1 s, then stuck in
        // the past for ever.
        readings: vec![0, 0, 1_000_000_000],
        index: std::cell::Cell::new(0),
    };
    let mut gcra = Gcra::with_clock(quota(), &clock);

    // Two: one at t=0, one at t=1 s. After that the schedule stands at
    // 1.1 s while the clock claims to be at zero, so every later request
    // is refused — the safe direction, and no permit is ever minted.
    //
    // This is also the one place GCRA and the token bucket disagree (the
    // bucket keeps its accumulated credit and grants eleven), which is
    // why their equivalence is stated only for monotonic clocks. See
    // `broken_clock_is_where_bucket_and_gcra_part_company` in
    // tests/comparison.rs.
    assert_eq!(drain(&mut gcra), 2);
}

// --- concurrency ---------------------------------------------------------

#[test]
fn concurrent_callers_share_one_tolerance_exactly() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(Gcra::with_clock(quota, clock)));

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
fn concurrent_recovery_is_exact() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(Gcra::with_clock(quota, Arc::clone(&clock))));

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

    assert_eq!(round(), 100);
    clock.advance(Duration::from_millis(250));
    assert_eq!(round(), 25);
}

#[test]
fn concurrent_batches_stay_all_or_nothing() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(Gcra::with_clock(quota, clock)));

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
    assert_eq!(granted, 33);
    assert_eq!(limiter.available(), 1);
}
