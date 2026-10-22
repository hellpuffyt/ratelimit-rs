//! `Keyed`: per-key limiters, eviction, and use from several threads.

use std::panic;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ratelimit::{Clock, Gcra, Keyed, ManualClock, Quota, Shared, SlidingWindow, TokenBucket};

type PerKeyBuckets = Keyed<u32, TokenBucket<Arc<ManualClock>>, Arc<ManualClock>>;

#[test]
fn keys_have_independent_budgets() {
    let clock = ManualClock::new();
    let quota = Quota::per_second(3).expect("valid");
    let mut keyed: Keyed<u32, TokenBucket<&ManualClock>, &ManualClock> =
        Keyed::with_clock(quota, &clock);
    for key in 0..100 {
        assert!(keyed.try_acquire_n(key, 3).is_allowed());
        assert!(keyed.try_acquire(key).is_denied());
    }
    assert_eq!(keyed.len(), 100);
}

#[test]
fn retain_active_keeps_every_key_still_being_limited() {
    let clock = ManualClock::new();
    let quota = Quota::per_second(10).expect("valid");
    let mut keyed: Keyed<u32, Gcra<&ManualClock>, &ManualClock> = Keyed::with_clock(quota, &clock);
    for key in 0..10 {
        assert!(keyed.try_acquire_n(key, key + 1).is_allowed());
    }
    // 500 ms later the keys that took <= 5 permits have recovered.
    clock.advance(Duration::from_millis(500));
    assert_eq!(keyed.retain_active(), 5);
    for key in 0..5 {
        assert!(!keyed.contains_key(&key));
    }
    for key in 5..10 {
        assert!(keyed.contains_key(&key), "key {key} evicted while limited");
    }
}

#[test]
fn eviction_never_changes_a_decision() {
    // Two identical maps driven by the same traffic; one evicts after
    // every request. Every decision must be identical.
    let clock = ManualClock::new();
    let quota = Quota::per_second(4).expect("valid");
    let mut evicting: Keyed<u8, SlidingWindow<&ManualClock>, &ManualClock> =
        Keyed::with_clock(quota, &clock);
    let mut keeping: Keyed<u8, SlidingWindow<&ManualClock>, &ManualClock> =
        Keyed::with_clock(quota, &clock);
    let mut state = 7_u64;
    for step in 0..5_000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let key = u8::try_from((state >> 40) % 4).expect("small");
        let permits = u32::try_from((state >> 50) % 3).expect("small");
        assert_eq!(
            evicting.try_acquire_n(key, permits),
            keeping.try_acquire_n(key, permits),
            "step {step}"
        );
        evicting.retain_active();
        clock.advance(Duration::from_millis((state >> 20) % 400));
    }
}

#[test]
fn shared_keyed_grants_exactly_each_keys_budget_across_threads() {
    let clock = Arc::new(ManualClock::new());
    let quota = Quota::per_second(25).expect("valid");
    let limiter: Arc<Shared<PerKeyBuckets>> =
        Arc::new(Shared::new(Keyed::with_clock(quota, Arc::clone(&clock))));

    let handles: Vec<_> = (0..8)
        .map(|t| {
            let limiter = Arc::clone(&limiter);
            thread::spawn(move || {
                let mut granted = [0_u32; 4];
                for i in 0..400_u32 {
                    let key = (i + t) % 4;
                    if limiter.with(|k| k.try_acquire(key)).is_allowed() {
                        granted[key as usize] += 1;
                    }
                }
                granted
            })
        })
        .collect();
    let mut totals = [0_u32; 4];
    for handle in handles {
        let granted = handle.join().expect("no panic");
        for (total, n) in totals.iter_mut().zip(granted) {
            *total += n;
        }
    }
    // A frozen clock: each of the 4 keys gets exactly its burst of 25,
    // however the 8 threads interleaved.
    assert_eq!(totals, [25; 4]);

    clock.advance(Duration::from_millis(200));
    let after: u32 = (0..4)
        .map(|key| {
            let mut n = 0;
            while limiter.with(|k| k.try_acquire(key)).is_allowed() {
                n += 1;
            }
            n
        })
        .sum();
    assert_eq!(after, 4 * 5, "200 ms at 25/s refills 5 per key");
}

#[test]
fn manual_clock_set_backwards_panics_without_moving_time() {
    let clock = ManualClock::starting_at(1_000);
    let result = panic::catch_unwind(|| clock.set_nanos(10));
    assert!(result.is_err());
    // Previously the new value was stored before the check fired.
    assert_eq!(clock.now_nanos(), 1_000);
}
