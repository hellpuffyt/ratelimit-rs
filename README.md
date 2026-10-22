# ratelimit-rs

[![CI](https://github.com/hellpuffyt/ratelimit-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/hellpuffyt/ratelimit-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ratelimit-rs.svg)](https://crates.io/crates/ratelimit-rs)

Four rate limiters — token bucket, GCRA, sliding window and fixed window — sharing one `Quota` type and one injectable `Clock`, with no runtime dependencies.

```toml
[dependencies]
ratelimit-rs = "0.1"
```

```rust
use std::time::Duration;
use ratelimit::{Decision, Quota, RateLimiter, TokenBucket};

// 100 requests per minute, but up to 10 may arrive at once.
let quota = Quota::per_minute(100)?.with_burst(10)?;
let mut limiter = TokenBucket::new(quota);

match limiter.try_acquire() {
    Decision::Allow { remaining } => println!("served; {remaining} left"),
    Decision::Deny { retry_after } => println!("429; retry in {retry_after:?}"),
    Decision::Exceeds { capacity } => println!("400; the most we ever allow is {capacity}"),
}
```

## Why this and not the other 1,000 rate limiters on GitHub

Almost every rate-limiter repo on GitHub is *one* algorithm — usually a token
bucket, sometimes with a name that promises more — wired straight to
`Instant::now()`, with a handful of tests that `thread::sleep` and hope the
scheduler is generous that day. That combination has three consequences this
crate is built specifically to avoid:

1. **You can't compare algorithms, because there's only one.** "Should I use
   a token bucket or a sliding window for this?" is usually answered by
   whichever crate you happened to `cargo add` first. This crate implements
   all four, behind one trait (`RateLimiter`) and one config type (`Quota`),
   specifically so the choice can be made on evidence instead of habit. The
   crate's own doc comment ships a table of exactly what each one lets
   through, and — this is the part that matters — **every cell in that table
   is a test, not a claim.** `tests/comparison.rs` drives all four limiters
   through identical schedules on the same manual clock and asserts the
   differences (and the one genuine equivalence: a token bucket and GCRA
   with matching parameters are proven, by exhaustive randomized testing, to
   return byte-identical decisions).

2. **Sleep-based tests can't check a boundary, only approximate one.** Every
   interesting bug in a rate limiter lives at an edge: one nanosecond either
   side of a window rollover, the instant a bucket is exactly empty, what
   happens when the burst is absorbed in a single call. A test that sleeps
   50ms and checks "roughly the right number of requests got through" cannot
   see any of that, and will not catch a regression that shifts a boundary
   by one unit. Every limiter here reads time through a `Clock` trait; tests
   drive a `ManualClock` and advance it by exact `Duration`s — including
   `Duration::from_nanos(1)` — so a boundary condition is asserted exactly,
   and the full test suite runs in well under a second with zero flakiness from
   the OS scheduler.

3. **"It compiles and doesn't panic" is not correctness for a rate limiter.**
   The interesting property is usually a *conservation law*: a token bucket
   must never let through more than `burst + rate * elapsed` permits, no
   matter how it's hammered, from however many threads, at however odd a
   pace. This crate has property-style tests for exactly that — burst load
   from up to 8 threads sharing one `Mutex`-guarded limiter, checked against
   the exact expected total for a frozen or precisely-advanced clock — plus
   explicit adversarial cases: a clock that goes backwards, a batch request
   larger than the whole burst, concurrent callers racing a window boundary.

A fair number of existing crates get pieces of this right. Fewer put all
four algorithms behind one abstraction with a machine-checked comparison
between them, and fewer still make the *testing method itself* — injected
time — part of the public API, so that a caller's own service code (the
retry path, the queue that backs up behind a 429) can be tested the same
deterministic way.

## The four algorithms, and when to reach for which

All four enforce the same long-run rate. They differ in what they let
through in the short run, which is the only thing that matters when
choosing between them:

| | Burst at one instant | Recovery after exhaustion | Worst case in any window-length span | State |
| --- | --- | --- | --- | --- |
| `TokenBucket` | `burst`, tunable | one permit every `period / count` | `burst + count` | 16 bytes |
| `Gcra` | `burst`, tunable | one permit every `period / count` | `burst + count` | 8 bytes |
| `SlidingWindow` | `count` | released continuously as the old window ages out | `2 * count` if a burst is aimed at the end of a window, less otherwise | 16 bytes |
| `FixedWindow` | `count` | nothing, then all of it at the boundary | `2 * count` | 16 bytes (12 + padding) |

- **`TokenBucket`** — the default. Absorbs the bursts real clients produce
  while holding the average, and `burst` is a dial you can set independently
  of the rate (`with_burst(1)` makes it a strict pacer that never allows two
  permits at once).
- **`Gcra`** — the same decisions as `TokenBucket` in half the state (one
  `u64` instead of two), and it knows *when* the next permit falls due
  (`next_permit_in`), which is what a scheduler wants instead of a bare
  allow/deny.
- **`SlidingWindow`** — when the limit is a promise about a named window
  ("600 per minute") that has to hold for *every* minute-long span, not just
  ones aligned to a clock boundary. Costs an approximation that errs toward
  refusing: traffic at exactly the permitted rate loses about one request in
  twenty (see below), because the weighted estimate assumes the previous
  window's permits were spread evenly through it.
- **`FixedWindow`** — when the boundary is part of the contract: a billing
  period, a daily allowance. Cheapest of the four, easiest to explain to a
  support ticket, and lets a client take `2 * count` across a boundary,
  which is fine when the boundary is real and unacceptable when it isn't.

### Real, observed output

The example below (`cargo run --example compare`) is the actual output the
crate produces — the same evidence `tests/comparison.rs` asserts, in a form
you can read:

```
$ cargo run --example compare
quota: 10 permits per second
emission interval: 100ms

== burst across a window boundary ==
(ask for 10 at t=0.9s, then 10 more at t=1.0s — 100 ms apart)

  token bucket     11 permits inside one second
  gcra             11 permits inside one second
  sliding window   10 permits inside one second
  fixed window     20 permits inside one second

== permits available after exhaustion at t=0 ==
  elapsed   bucket  gcra  sliding  fixed
      0ns        0     0        0      0
    100ms        1     1        0      0
    200ms        2     2        0      0
    300ms        3     3        0      0
    400ms        4     4        0      0
    500ms        5     5        0      0
    600ms        6     6        0      0
    700ms        7     7        0      0
    800ms        8     8        0      0
    900ms        9     9        0      0
       1s       10    10        0     10

== one request every 100 ms for 10 seconds ==
  token bucket     100 of 100 allowed
  gcra             100 of 100 allowed
  sliding window    95 of 100 allowed
  fixed window     100 of 100 allowed

(the sliding window is the odd one out, and this is its real cost:
 traffic at exactly the permitted rate loses one request per window,
 because the weighted estimate assumes the previous window's permits
 were spread evenly through it. It errs towards refusing.)
```

`fixed window` doubling its permits across the boundary, and `token bucket`
/ `gcra` doing the same thing for a different reason (the burst was tunable
down to 1, the window's wasn't) is the whole comparison in one paragraph —
and it's a run of the actual code, not a claim about it.

## Time is injected

No limiter calls `std::time::Instant::now()` internally. They read a
`Clock` trait, which in production is `SystemClock` and in tests is
`ManualClock`:

```rust
use std::time::Duration;
use ratelimit::{Clock, ManualClock};

let clock = ManualClock::new();
assert_eq!(clock.now_nanos(), 0);
clock.advance(Duration::from_millis(250));
assert_eq!(clock.now_nanos(), 250_000_000);
```

The same mechanism is available to you. If your service's behaviour under a
rate limit is worth testing — the retry path, the 429 handler, the queue
that backs up — drive your limiter from a `ManualClock` and test it in
microseconds instead of `tokio::time::sleep`.

## Shared and per-key use

Limiters take `&mut self`, so sharing is explicit:

```rust
use ratelimit::{Shared, TokenBucket, Quota};
let limiter = Shared::new(TokenBucket::new(Quota::per_second(100)?));
// limiter.try_acquire() now takes &self, safe to call from any thread.
```

`Keyed<K, L, C>` builds one limiter per key on first sight of it, for the
much more common real shape of a rate limit — "10 per second *each*", not
"1000 per second total." It composes with `Shared`:
`Shared<Keyed<IpAddr, Gcra<Arc<ManualClock>>, Arc<ManualClock>>>` is a
per-IP limiter usable from a thread pool. `Keyed::retain_active` evicts keys
whose limiters have fully recovered, so a naive per-IP table doesn't become
a memory-exhaustion vector in its own right.

## Correctness under burst load

`tests/token_bucket.rs`, `tests/gcra.rs`, `tests/sliding_window.rs` and
`tests/fixed_window.rs` each carry the same shape of coverage: exhaustion,
recovery, boundary nanoseconds, batch (`_n`) requests, a backwards clock,
and — the part that actually earns the word "correctness" here — concurrent
access. `Shared<L>` is hammered from up to 8 threads at once (a mix of
single- and multi-permit requests) against a frozen or precisely-advanced
clock, and the number of permits granted is checked against the *exact*
expected total for that interleaving, not a range or "didn't over-allow by
much." `tests/comparison.rs` adds 20,000-step randomized-schedule tests
proving `TokenBucket` and `Gcra`
return byte-identical decisions — including the exact `retry_after` on every
refusal — for any clock that keeps the documented monotonic contract, and
shows exactly where that equivalence breaks (a clock that goes backwards).

`tests/properties.rs` adds randomized property tests over reproducible
seeds: every limiter is checked against an independent reference model
(the window models recompute their counts from a full log of grants),
every `retry_after` is checked to be exact (waiting that long succeeds,
one nanosecond less does not), a clock that steps backwards is checked
never to mint permits, and quotas and clock readings at the extremes of
the `u64` nanosecond range are checked never to overflow or fail open.

Run it yourself:

```
$ cargo test --all-targets
$ cargo test --doc
```

At the time of the last audit that was 171 passing tests across the
unit/integration targets plus 12 doctests, 0 failed, 0 ignored (see
[`docs/PRODUCTION_READINESS.md`](docs/PRODUCTION_READINESS.md)).

## No dependencies

The crate is `std`-only: no optional features, nothing to audit in a
`cargo tree`. Rate limiting sits in front of everything else a service does,
so it's a poor place to inherit a dependency tree — a supply-chain
compromise in a rate limiter's dependency is a compromise of the thing that
was supposed to be your first line of defense.

## MSRV

Rust 1.70, checked in CI.

## Limitations

- GCRA cannot represent a schedule that runs past `u64::MAX` nanoseconds
  of its clock (over 584 years after the clock's epoch). A request whose
  schedule would end there is refused with `retry_after: Duration::MAX`
  rather than admitted — it fails closed. `SystemClock` starts at zero
  when created, so this only concerns custom clocks with a far-future
  epoch.
- A clock that steps backwards is absorbed, never rewarded: the bucket
  and GCRA ignore the lost time, and the window limiters stay in the
  latest window they have seen (the sliding window charges the previous
  window as if at the start of that window). `retry_after` values given
  while the clock is behind are measured from the backwards reading.
- `Shared` is a plain `Mutex`; there is no lock-free variant.

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## License

MIT. See [LICENSE](LICENSE).
