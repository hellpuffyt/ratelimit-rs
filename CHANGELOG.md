# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-01

### Added

- `Quota`: a rate (`count` per `period`) with a tunable `burst`, shared
  configuration for all four limiters.
- `TokenBucket`: continuous refill, tunable burst independent of rate.
- `Gcra`: the generic cell rate algorithm — the same admission decisions as
  `TokenBucket` in half the state (one `u64` timestamp instead of two
  counters), plus `next_permit_in` for scheduling.
- `SlidingWindow`: a weighted estimate across the current and previous fixed
  windows, bounding worst-case burst to under `2 * count` instead of the
  fixed window's full `2 * count`.
- `FixedWindow`: clock-aligned counting windows, cheapest of the four.
- `Clock` trait with `SystemClock` (production) and `ManualClock` (tests):
  every limiter reads time through this trait rather than calling
  `Instant::now()` directly, which is what makes exact-nanosecond boundary
  tests possible without sleeping.
- `Shared<L>`: a `Mutex`-guarded wrapper giving `&self` access to any
  limiter for use from multiple threads.
- `Keyed<K, L, C>`: one limiter per key, created lazily, with
  `retain_active` to evict fully-recovered keys and bound memory use.
- `Decision` (`Allow` / `Deny` / `Exceeds`) and the `RateLimiter` trait
  implemented by all four algorithms, plus `_n` (batch) and `peek_n`
  variants of every operation.
- `examples/compare.rs`: runs all four algorithms side by side on the
  traffic that separates them and prints the real numbers.
- 151 tests: per-algorithm unit tests (exhaustion, recovery, boundary
  nanoseconds, batch requests, backwards clocks, concurrent access from up
  to 8 threads checked against exact expected totals) plus a
  `tests/comparison.rs` suite that pins every claim the crate's docs make
  about how the four algorithms differ.

### Notes

- No runtime dependencies.
- MSRV: Rust 1.70.

[0.1.0]: https://github.com/hellpuffyt/ratelimit-rs/releases/tag/v0.1.0
