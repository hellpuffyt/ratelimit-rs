//! Fixed-window behaviour, including the boundary burst it is famous
//! for. The flaw is asserted, not apologised for: a limiter's failure
//! mode is part of its contract.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ratelimit::{Decision, FixedWindow, ManualClock, Quota, RateLimiter, Shared};

/// Ten permits per one-second window.
fn quota() -> Quota {
    Quota::per_second(10).expect("valid quota")
}

fn window(clock: &ManualClock) -> FixedWindow<&ManualClock> {
    FixedWindow::with_clock(quota(), clock)
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
fn starts_with_the_whole_budget() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    assert_eq!(window.available(), 10);
    assert_eq!(window.used(), 0);
    assert!(window.is_idle());
}

#[test]
fn whole_budget_is_available_at_one_instant() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    assert_eq!(drain(&mut window), 10);
}

#[test]
fn quota_is_reported_back() {
    let clock = ManualClock::new();
    assert_eq!(window(&clock).quota(), quota());
}

#[test]
fn burst_is_ignored() {
    let clock = ManualClock::new();
    let quota = quota().with_burst(1).expect("valid burst");
    let mut window = FixedWindow::with_clock(quota, &clock);
    assert_eq!(drain(&mut window), 10);
}

#[test]
fn used_counts_up_as_permits_are_taken() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for taken in 1..=10 {
        window.try_acquire();
        assert_eq!(window.used(), taken);
    }
}

// --- the boundary burst --------------------------------------------------

#[test]
fn a_client_gets_twice_the_limit_across_a_boundary() {
    // The documented flaw, asserted so that it cannot be "fixed" by
    // accident: 20 permits inside a single one-second span, without ever
    // breaking the limit as this limiter defines it.
    let clock = ManualClock::new();
    let mut window = window(&clock);

    clock.advance(Duration::from_millis(900));
    assert_eq!(drain(&mut window), 10);

    clock.advance(Duration::from_millis(100));
    assert_eq!(drain(&mut window), 10, "a brand new window, 100 ms later");
}

#[test]
fn a_greedy_client_starting_on_a_boundary_gets_only_the_limit() {
    // The doubling is not automatic: a client whose burst lands exactly
    // on a boundary and then retries constantly gets the limit and
    // nothing more, because its two clusters are a full window apart.
    // The doubling needs the burst to fall *inside* a window, which is
    // what the test above — and any client that did not start at
    // second zero — does. See `burst_offset_sweep` in
    // tests/comparison.rs for the full picture.
    let clock = ManualClock::new();
    let mut window = window(&clock);

    let mut grants_at_ms = Vec::new();
    for ms in 0..5_000_u64 {
        while window.try_acquire().is_allowed() {
            grants_at_ms.push(ms);
        }
        clock.advance(Duration::from_millis(1));
    }

    let busiest = grants_at_ms
        .iter()
        .map(|&start| {
            grants_at_ms
                .iter()
                .filter(|&&at| at >= start && at < start + 1_000)
                .count()
        })
        .max()
        .expect("some permits were granted");

    assert_eq!(busiest, 10, "two clusters, exactly one window apart");
}

#[test]
fn windows_are_aligned_to_the_clock_epoch() {
    // Not to the first request: two limiters sharing a clock must agree
    // on where the boundaries are.
    let clock = ManualClock::starting_at(600_000_000);
    let mut window = window(&clock);
    drain(&mut window);

    // 400 ms into the run is 1.0 s on the clock — a boundary.
    clock.advance(Duration::from_millis(399));
    assert_eq!(window.available(), 0);
    clock.advance(Duration::from_millis(1));
    assert_eq!(window.available(), 10);
}

#[test]
fn window_remaining_counts_down_to_the_boundary() {
    let clock = ManualClock::new();
    let window = window(&clock);
    assert_eq!(window.window_remaining(), Duration::from_secs(1));

    clock.advance(Duration::from_millis(250));
    assert_eq!(window.window_remaining(), Duration::from_millis(750));

    clock.advance(Duration::from_millis(750));
    assert_eq!(window.window_remaining(), Duration::from_secs(1));
}

// --- recovery is a cliff, not a slope ------------------------------------

#[test]
fn nothing_is_returned_until_the_boundary() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    for tenth in 1..10 {
        clock.advance(Duration::from_millis(100));
        assert_eq!(window.available(), 0, "{tenth} tenths into the window");
    }
    clock.advance(Duration::from_millis(100));
    assert_eq!(window.available(), 10, "all of it, all at once");
}

#[test]
fn one_nanosecond_either_side_of_the_boundary() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    clock.advance(Duration::from_secs(1) - Duration::from_nanos(1));
    assert!(window.try_acquire().is_denied());
    clock.advance(Duration::from_nanos(1));
    assert!(window.try_acquire().is_allowed());
}

#[test]
fn a_skipped_window_is_not_credited() {
    // Idling through five windows does not bank five windows' worth.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    clock.advance(Duration::from_secs(5));
    assert_eq!(drain(&mut window), 10);
}

#[test]
fn retry_after_points_at_the_boundary() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    clock.advance(Duration::from_millis(250));
    drain(&mut window);

    assert_eq!(
        window.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_millis(750)
        }
    );
}

#[test]
fn retry_after_is_the_same_for_one_permit_or_the_whole_budget() {
    // Nothing is available until the reset, and at the reset everything
    // is, so the wait does not depend on the size of the request.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    assert_eq!(
        window.try_acquire().retry_after(),
        window.try_acquire_n(10).retry_after()
    );
}

#[test]
fn waiting_exactly_the_advertised_time_succeeds() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    clock.advance(Duration::from_millis(377));
    drain(&mut window);

    let retry = window.try_acquire_n(10).retry_after().expect("a wait");
    clock.advance(retry - Duration::from_nanos(1));
    assert!(window.try_acquire_n(10).is_denied());
    clock.advance(Duration::from_nanos(1));
    assert!(window.try_acquire_n(10).is_allowed());
}

#[test]
fn is_idle_only_between_windows() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    window.try_acquire();
    assert!(!window.is_idle());
    clock.advance(Duration::from_millis(999));
    assert!(!window.is_idle());
    clock.advance(Duration::from_millis(1));
    assert!(window.is_idle());
}

#[test]
fn reset_clears_the_counter() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    window.reset();
    assert_eq!(window.available(), 10);
    assert_eq!(window.used(), 0);
}

// --- steady state --------------------------------------------------------

#[test]
fn traffic_at_exactly_the_rate_is_never_refused() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for i in 0..1_000 {
        assert!(window.try_acquire().is_allowed(), "request {i}");
        clock.advance(Duration::from_millis(100));
    }
}

#[test]
fn traffic_above_the_rate_settles_to_the_rate() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    let mut allowed = 0;
    for _ in 0..10_000 {
        if window.try_acquire().is_allowed() {
            allowed += 1;
        }
        clock.advance(Duration::from_millis(1));
    }
    // Ten windows of ten, and no more — the doubling is a property of
    // one boundary, not of the long-run rate.
    assert_eq!(allowed, 100);
}

// --- batches, and requests that never fit --------------------------------

#[test]
fn a_batch_is_all_or_nothing() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    window.try_acquire_n(8);
    assert!(window.try_acquire_n(5).is_denied());
    assert_eq!(window.used(), 8);
}

#[test]
fn the_whole_budget_can_be_taken_in_one_batch() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    assert_eq!(window.try_acquire_n(10), Decision::Allow { remaining: 0 });
}

#[test]
fn asking_for_more_than_the_budget_can_never_succeed() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    assert_eq!(window.try_acquire_n(11), Decision::Exceeds { capacity: 10 });
    clock.advance(Duration::from_secs(3600));
    assert_eq!(window.try_acquire_n(11), Decision::Exceeds { capacity: 10 });
    assert_eq!(window.used(), 0);
}

#[test]
fn zero_permits_are_always_allowed() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    assert_eq!(window.try_acquire_n(0), Decision::Allow { remaining: 0 });
    assert_eq!(window.used(), 10);
}

// --- peeking -------------------------------------------------------------

#[test]
fn peek_does_not_consume() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for _ in 0..100 {
        assert!(window.peek().is_allowed());
    }
    assert_eq!(window.used(), 0);
}

#[test]
fn peek_matches_the_decision_that_follows_it() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for _ in 0..40 {
        let peeked = window.peek_n(2);
        assert_eq!(peeked, window.try_acquire_n(2));
        clock.advance(Duration::from_millis(70));
    }
}

// --- concurrency ---------------------------------------------------------

#[test]
fn concurrent_callers_share_one_window_exactly() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(FixedWindow::with_clock(quota, clock)));

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
fn concurrent_callers_see_exactly_one_reset() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(FixedWindow::with_clock(
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

    assert_eq!(round(), 100);
    clock.advance(Duration::from_millis(500));
    assert_eq!(round(), 0, "still the same window");
    clock.advance(Duration::from_millis(500));
    assert_eq!(round(), 100, "and exactly one window's worth after it");
}
