//! Sliding-window behaviour: the boundary it fixes, and the
//! approximation it pays for the fix.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ratelimit::{Decision, ManualClock, Quota, RateLimiter, Shared, SlidingWindow};

/// Ten permits per one-second window.
fn quota() -> Quota {
    Quota::per_second(10).expect("valid quota")
}

fn window(clock: &ManualClock) -> SlidingWindow<&ManualClock> {
    SlidingWindow::with_clock(quota(), clock)
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
    // A window counter's burst allowance is its whole budget, so the
    // quota's burst field cannot change anything here. Asserted because
    // it is documented.
    let clock = ManualClock::new();
    let quota = quota().with_burst(1).expect("valid burst");
    let mut window = SlidingWindow::with_clock(quota, &clock);
    assert_eq!(drain(&mut window), 10);
}

#[test]
fn epoch_offset_does_not_change_behaviour() {
    let clock = ManualClock::starting_at(7_500_000_000);
    let mut window = window(&clock);
    assert_eq!(drain(&mut window), 10);
    clock.advance(Duration::from_secs(2));
    assert_eq!(drain(&mut window), 10);
}

// --- the boundary, which is the whole point ------------------------------

#[test]
fn the_fixed_window_boundary_trick_does_not_work() {
    // Spend everything just before a boundary, then try to spend it all
    // again just after. A fixed window allows all 20; this must not.
    let clock = ManualClock::new();
    let mut window = window(&clock);

    clock.advance(Duration::from_millis(900));
    assert_eq!(drain(&mut window), 10);

    clock.advance(Duration::from_millis(100));
    assert_eq!(drain(&mut window), 0, "the previous window still counts");
}

#[test]
fn the_previous_window_is_charged_in_proportion() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    assert_eq!(drain(&mut window), 10);

    // Ten permits in window 0. One second later, the whole of window 0
    // is still inside the trailing second, so nothing is free; a tenth
    // of a second later, a tenth of it has aged out.
    for tenths in 0..=10 {
        clock.advance(Duration::from_millis(if tenths == 0 { 1000 } else { 100 }));
        assert_eq!(
            window.available(),
            tenths,
            "{tenths} tenths past the boundary"
        );
    }
}

#[test]
fn budget_returns_gradually_rather_than_at_the_boundary() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    clock.advance(Duration::from_secs(1));
    assert_eq!(window.available(), 0, "nothing at the boundary itself");

    let mut regained = Vec::new();
    for _ in 0..10 {
        clock.advance(Duration::from_millis(100));
        regained.push(window.available());
    }
    assert_eq!(regained, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
}

#[test]
fn a_greedy_client_never_beats_the_limit() {
    // Ask constantly for five windows and take the busiest
    // window-length span. A fixed window hands a client that does
    // exactly this a clean `2 * limit` (asserted in
    // tests/fixed_window.rs).
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

    // Not merely below the fixed window's clean doubling: a client that
    // simply asks as fast as it can never beats the limit at all.
    assert_eq!(busiest, 10, "worst one-second span under constant attack");
}

#[test]
fn a_crafted_burst_at_the_window_edge_can_still_double() {
    // Greed is not the worst case; timing is. Spend the whole budget in
    // the last millisecond of a window — where the weighting discounts
    // it fastest — and then ask constantly. This reaches the same
    // `2 * limit` the fixed window gives away for free.
    //
    // Which is the honest summary of what the weighted counter buys: not
    // a smaller worst case, but a worst case that has to be *aimed* at
    // the window edge instead of arriving by accident.
    let clock = ManualClock::new();
    let mut window = window(&clock);

    let mut grants_at_ms = Vec::new();
    clock.advance(Duration::from_millis(999));
    while window.try_acquire().is_allowed() {
        grants_at_ms.push(999_u64);
    }
    for ms in 1_000..3_000_u64 {
        clock.advance(Duration::from_millis(1));
        while window.try_acquire().is_allowed() {
            grants_at_ms.push(ms);
        }
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

    assert_eq!(busiest, 20, "worst one-second span under a crafted burst");
}

#[test]
fn stale_windows_are_forgotten_entirely() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    // Two whole windows later, nothing from window 0 is inside the
    // trailing window under any weighting.
    clock.advance(Duration::from_secs(2));
    assert!(window.is_idle());
    assert_eq!(drain(&mut window), 10);
}

#[test]
fn a_long_silence_is_forgotten() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    clock.advance(Duration::from_secs(86_400));
    assert_eq!(drain(&mut window), 10);
}

// --- the approximation, stated honestly ----------------------------------

#[test]
fn it_charges_for_permits_that_have_already_aged_out() {
    // Ten permits at t=0. At t=1.5 s the trailing second is [0.5, 1.5],
    // which contains none of them — an exact log would report ten
    // available. The weighted estimate assumes they were spread evenly
    // through window 0, so it still charges five. It errs towards
    // refusing, which is the safe direction for a limiter.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    clock.advance(Duration::from_millis(1_500));
    assert_eq!(window.weighted_used(), 5);
    assert_eq!(window.available(), 5);
}

#[test]
fn traffic_at_exactly_the_rate_loses_one_request_per_window() {
    // The price of the approximation, measured: one request per window
    // is refused even though the client is exactly on its limit.
    let clock = ManualClock::new();
    let mut window = window(&clock);

    let mut allowed = 0;
    for _ in 0..100 {
        if window.try_acquire().is_allowed() {
            allowed += 1;
        }
        clock.advance(Duration::from_millis(100));
    }
    assert_eq!(allowed, 95);
}

#[test]
fn traffic_below_the_rate_is_never_refused() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for i in 0..500 {
        assert!(window.try_acquire().is_allowed(), "request {i}");
        clock.advance(Duration::from_millis(125));
    }
}

// --- retry_after ---------------------------------------------------------

#[test]
fn retry_after_is_exact_when_the_current_window_has_room() {
    // Nothing spent in this window, but the previous one is full. The
    // wait is however long it takes the previous window to age out far
    // enough — to the nanosecond.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    clock.advance(Duration::from_millis(900));
    drain(&mut window);
    clock.advance(Duration::from_millis(100));

    assert_eq!(
        window.try_acquire_n(5),
        Decision::Deny {
            retry_after: Duration::from_nanos(400_000_001)
        }
    );
}

#[test]
fn retry_after_is_exact_across_a_boundary() {
    // The whole budget was spent in *this* window, so no amount of
    // ageing helps until the boundary — and then only once this window's
    // count, which becomes the previous count, has itself aged out.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);

    assert_eq!(
        window.try_acquire(),
        Decision::Deny {
            retry_after: Duration::from_nanos(1_000_000_001)
        }
    );
}

#[test]
fn waiting_exactly_the_advertised_time_succeeds() {
    for permits in 1..=10 {
        let clock = ManualClock::new();
        let mut window = window(&clock);
        drain(&mut window);

        let retry = window
            .try_acquire_n(permits)
            .retry_after()
            .expect("a denial carries a wait");
        clock.advance(retry);
        assert!(
            window.try_acquire_n(permits).is_allowed(),
            "{permits} permits after the advertised wait"
        );
    }
}

#[test]
fn one_nanosecond_short_of_the_advertised_time_still_fails() {
    for permits in 1..=10 {
        let clock = ManualClock::new();
        let mut window = window(&clock);
        drain(&mut window);

        let retry = window.try_acquire_n(permits).retry_after().expect("a wait");
        clock.advance(retry - Duration::from_nanos(1));
        assert!(
            window.try_acquire_n(permits).is_denied(),
            "{permits} permits one nanosecond early"
        );
    }
}

#[test]
fn retry_after_is_never_zero_on_a_denial() {
    // A denial advertising a zero wait would spin a caller that trusts
    // it. Checked across a whole window at nanosecond-scale offsets.
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for step in 0..500 {
        clock.advance(Duration::from_nanos(1 + step * 4_000_000));
        while window.try_acquire().is_allowed() {}
        let retry = window.try_acquire().retry_after().expect("a wait");
        assert!(retry > Duration::ZERO, "at step {step}");
    }
}

// --- batches, and requests that never fit --------------------------------

#[test]
fn a_batch_is_all_or_nothing() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    window.try_acquire_n(8);
    assert!(window.try_acquire_n(5).is_denied());
    assert_eq!(window.available(), 2);
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
    assert_eq!(window.available(), 10);
}

#[test]
fn zero_permits_are_always_allowed() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    drain(&mut window);
    assert_eq!(window.try_acquire_n(0), Decision::Allow { remaining: 0 });
    assert_eq!(window.weighted_used(), 10);
}

// --- peeking and resetting -----------------------------------------------

#[test]
fn peek_does_not_consume() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    for _ in 0..100 {
        assert!(window.peek().is_allowed());
    }
    assert_eq!(window.available(), 10);
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

#[test]
fn reset_clears_both_windows() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    clock.advance(Duration::from_millis(900));
    drain(&mut window);
    clock.advance(Duration::from_millis(200));

    assert_eq!(window.available(), 1);
    window.reset();
    assert_eq!(window.available(), 10);
    assert!(window.is_idle());
}

#[test]
fn is_idle_tracks_the_weighted_estimate() {
    let clock = ManualClock::new();
    let mut window = window(&clock);
    window.try_acquire();
    assert!(!window.is_idle());

    // One permit weighs less than one whole permit once 90% of its
    // window has aged out, and the floored estimate reaches zero.
    clock.advance(Duration::from_millis(1_100));
    assert!(window.is_idle());
}

// --- concurrency ---------------------------------------------------------

#[test]
fn concurrent_callers_share_one_budget_exactly() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(SlidingWindow::with_clock(quota, clock)));

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
}

#[test]
fn concurrent_callers_see_the_window_slide_exactly() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(100).expect("valid quota");
    let limiter = Arc::new(Shared::new(SlidingWindow::with_clock(
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
    // Into the next window, 30% of the way: 70% of the previous window
    // is still charged, leaving 30 permits.
    clock.advance(Duration::from_millis(1_300));
    assert_eq!(round(), 30);
}
