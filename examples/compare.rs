//! Prints the differences between the four algorithms, on the traffic
//! that actually separates them.
//!
//! Run with `cargo run --example compare`. Every number it prints is
//! produced by the library itself against a `ManualClock`, so this is the
//! same evidence the test suite asserts, in a form you can read.

use std::sync::Arc;
use std::time::Duration;

use ratelimit::{FixedWindow, Gcra, ManualClock, Quota, RateLimiter, SlidingWindow, TokenBucket};

/// Every limiter is held to the same quota: ten permits per second.
const LIMIT: u32 = 10;

/// The clock is shared through an `Arc` rather than a borrow so that a
/// limiter's *type* does not depend on a lifetime — which is what lets
/// the scenarios below be written once and run against all four
/// algorithms.
type Shared = Arc<ManualClock>;

fn main() {
    let quota = Quota::per_second(LIMIT).expect("10 per second is a valid quota");

    println!("quota: {LIMIT} permits per second");
    println!("emission interval: {:?}\n", quota.emission_interval());

    boundary_burst(quota);
    recovery_shape(quota);
    steady_state(quota);
}

/// Runs `scenario` against a limiter built by `build`, on a fresh manual
/// clock the scenario also gets to drive.
fn run<L: RateLimiter>(
    quota: Quota,
    build: fn(Quota, Shared) -> L,
    scenario: fn(&ManualClock, L) -> usize,
) -> usize {
    let clock = Arc::new(ManualClock::new());
    let limiter = build(quota, Arc::clone(&clock));
    scenario(&clock, limiter)
}

/// The classic fixed-window flaw: spend the budget at the end of one
/// window and again at the start of the next, and twice the limit lands
/// inside a single second.
fn boundary_burst(quota: Quota) {
    println!("== burst across a window boundary ==");
    println!("(ask for 10 at t=0.9s, then 10 more at t=1.0s — 100 ms apart)\n");

    for (name, allowed) in [
        (
            "token bucket",
            run(quota, TokenBucket::with_clock, straddle),
        ),
        ("gcra", run(quota, Gcra::with_clock, straddle)),
        (
            "sliding window",
            run(quota, SlidingWindow::with_clock, straddle),
        ),
        (
            "fixed window",
            run(quota, FixedWindow::with_clock, straddle),
        ),
    ] {
        println!("  {name:<16} {allowed:>2} permits inside one second");
    }
    println!();
}

fn straddle<L: RateLimiter>(clock: &ManualClock, mut limiter: L) -> usize {
    clock.advance(Duration::from_millis(900));
    let first = (0..LIMIT)
        .filter(|_| limiter.try_acquire().is_allowed())
        .count();

    clock.advance(Duration::from_millis(100));
    let second = (0..LIMIT)
        .filter(|_| limiter.try_acquire().is_allowed())
        .count();

    first + second
}

/// How permits come back after the limiter has been drained: a drip, a
/// gradual release, or a cliff.
fn recovery_shape(quota: Quota) {
    println!("== permits available after exhaustion at t=0 ==");
    println!("  elapsed   bucket  gcra  sliding  fixed");

    let clock = ManualClock::new();
    let mut bucket = TokenBucket::with_clock(quota, &clock);
    let mut gcra = Gcra::with_clock(quota, &clock);
    let mut sliding = SlidingWindow::with_clock(quota, &clock);
    let mut fixed = FixedWindow::with_clock(quota, &clock);

    for _ in 0..LIMIT {
        bucket.try_acquire();
        gcra.try_acquire();
        sliding.try_acquire();
        fixed.try_acquire();
    }

    for step in 0..=10 {
        if step > 0 {
            clock.advance(Duration::from_millis(100));
        }
        println!(
            "  {:>7?}   {:>6}  {:>4}  {:>7}  {:>5}",
            Duration::from_millis(step * 100),
            bucket.available(),
            gcra.available(),
            sliding.available(),
            fixed.available(),
        );
    }
    println!();
}

/// Traffic at exactly the sustained rate. Three of the four never refuse
/// it; the sliding window's approximation costs it one request per
/// window, which is the price of not keeping a log.
fn steady_state(quota: Quota) {
    println!("== one request every 100 ms for 10 seconds ==");

    for (name, allowed) in [
        ("token bucket", run(quota, TokenBucket::with_clock, paced)),
        ("gcra", run(quota, Gcra::with_clock, paced)),
        (
            "sliding window",
            run(quota, SlidingWindow::with_clock, paced),
        ),
        ("fixed window", run(quota, FixedWindow::with_clock, paced)),
    ] {
        println!("  {name:<16} {allowed:>3} of 100 allowed");
    }
    println!();
    println!("(the sliding window is the odd one out, and this is its real cost:");
    println!(" traffic at exactly the permitted rate loses one request per window,");
    println!(" because the weighted estimate assumes the previous window's permits");
    println!(" were spread evenly through it. It errs towards refusing.)");
}

fn paced<L: RateLimiter>(clock: &ManualClock, mut limiter: L) -> usize {
    let mut allowed = 0;
    for _ in 0..100 {
        if limiter.try_acquire().is_allowed() {
            allowed += 1;
        }
        clock.advance(Duration::from_millis(100));
    }
    allowed
}
