# Production readiness — ratelimit-rs

Audit date: 2026-10-22. Branch: `audit/production-readiness`.
Tested code commit: `4bb1a26` (later commits on the branch change only
documentation and `Cargo.toml` metadata; the full suite was re-run on the
final commit with the same counts).

## Scope

A dependency-free (`std`-only) Rust library with four rate limiters —
`TokenBucket`, `Gcra`, `SlidingWindow`, `FixedWindow` — sharing one
`Quota` type and an injectable `Clock` (`SystemClock` / `ManualClock`),
plus `Shared<L>` (mutex wrapper for `&self` use from threads) and
`Keyed<K, L, C>` (one limiter per key with `retain_active` eviction).
There is no binary, no I/O, no network and no persisted state.

Architecture: every limiter is integer arithmetic over `u64` nanoseconds
read from `Clock::now_nanos`; `Quota` normalises to an emission interval
and rejects configurations whose bucket depth would overflow `u64`.

## Platforms and prerequisites

- Rust stable with `rustfmt` and `clippy`. Declared MSRV 1.70 (checked in
  CI; the oldest toolchain available on the audit host was 1.85, where
  `cargo +1.85 build --all-targets` passed — 1.70 itself was not run
  locally).
- Verified locally on macOS (Apple silicon). CI runs Linux, macOS and
  Windows; the code has no platform-specific paths.

## Clean-clone verification

```
git clone -b audit/production-readiness <repo> ratelimit-rs && cd ratelimit-rs
cargo build --all-targets            # exit 0
cargo fmt --all -- --check           # exit 0
cargo clippy --all-targets -- -D warnings   # exit 0
cargo test --all-targets             # exit 0 — 171 passed, 0 failed, 0 ignored
cargo test --doc                     # exit 0 — 12 passed, 0 failed, 0 ignored
cargo run --example compare          # exit 0 — output identical to tests/fixtures/compare-output.txt
```

The README's install line was also exercised: a scratch crate depending
on the branch via `git = ...` built and ran the README example
(`served; 9 left`).

## Commands and results (audit host)

| Command | Exit | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 | no warnings |
| `cargo test --all-targets` (baseline, before fixes) | 0 | 151 passed, 0 failed, 0 ignored |
| `cargo test --all-targets` (after) | 0 | 171 passed, 0 failed, 0 ignored |
| `cargo test --doc` | 0 | 12 passed, 0 failed, 0 ignored |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | 0 | clean |
| `cargo audit --db <local advisory-db> --no-fetch` | 0 | 0 vulnerabilities (no dependencies) |
| `cargo deny check` | — | not configured in this repo (no `deny.toml`) |
| new property tests against the *unfixed* source | 101 | 3 of 12 failed (the defects below) |

Test breakdown after the audit: unit 14, `tests/token_bucket.rs` 41,
`tests/gcra.rs` 39, `tests/sliding_window.rs` 30, `tests/fixed_window.rs`
27, `tests/properties.rs` 15 (new), `tests/keyed.rs` 5 (new); doctests 12.

## CI status

Default branch: `success` (run on `2ba35d6`, 2026-09-01).
Audit branch: pending until pushed. CI actions are referenced by tag
(`@v4`, `@v2`, `@stable`), not commit SHA; left unchanged because they
are not broken.

## Bugs found and fixed (each with a regression test)

| # | Severity | Defect | Fix | Regression test |
| --- | --- | --- | --- | --- |
| 1 | High (fail-open) | `Gcra`: when `tat + cost` overflowed `u64` the TAT saturated, the same value was stored on every call, and **every** request was admitted (clock readings near `u64::MAX`, e.g. a custom clock with a far-future epoch). `next_permit_in` under-reported likewise. | `checked_add`; refuse with `retry_after: Duration::MAX`; `delay_for` in `u128`. | `properties::gcra_at_the_end_of_the_clock_range_refuses_instead_of_admitting_everything`, `extreme_quotas_and_clock_readings_never_overflow_or_fail_open` |
| 2 | Medium (fail-open) | `FixedWindow` / `SlidingWindow`: a clock reading from an earlier window was treated as a new window and cleared the counters, minting a fresh budget — contrary to the `Clock` docs' promise that backwards readings are absorbed. | Only roll forward; sliding window uses offset 0 (most conservative) when behind; `window_remaining` measured to the end of the stored window. | `properties::{fixed,sliding}_window_survives_a_backwards_clock`, `window_limiters_do_not_reset_when_the_clock_steps_back_a_window` |
| 3 | Low | `SlidingWindow::retry_after` overflowed for periods near `u64::MAX`: wrapped to a tiny wait in release, panicked in debug. | `saturating_add`. | `properties::sliding_window_retry_after_saturates_instead_of_wrapping` |
| 4 | Low (test helper) | `ManualClock::set_nanos` stored the backwards value *before* its assertion panicked. | Check-then-CAS loop. | `keyed::manual_clock_set_backwards_panics_without_moving_time` |
| 5 | Docs (install hazard) | README told users to add `ratelimit-rs = "0.1"`; that crates.io name belongs to an **unrelated crate**, so following the README fetched someone else's code. Badge and docs.rs link pointed at it too. | README now uses a `git` dependency; badge and docs.rs link removed. | Scratch consumer crate built from the branch (above). |
| 6 | Docs | Comparison table claimed the sliding window's worst case is "under `2 * count`" while `tests/sliding_window.rs` asserts exactly 20 for count 10 under an aimed burst; window state listed as 12 bytes where `SlidingWindow` measures 16. README quoted cargo output that cargo does not print. | Table and text corrected to what the tests assert. | Existing `sliding_window` and `per_limiter_state_is_what_the_documentation_claims` tests. |

## Security review

- No dependencies; `#![forbid(unsafe_code)]`; no I/O.
- Resource exhaustion: `Keyed` grows per key until `retain_active` is
  called (documented); eviction never changes a decision (tested).
- Arithmetic: all overflow paths found by the extreme-value test are now
  fail-closed; tests run with overflow checks on.
- Secret scan of the tree and full history (token/key/private-key/path
  patterns and generic `key|secret|password|token = "..."`): no hits.
- Workflow permissions: `contents: read`.

## Known limitations and unresolved risks

- GCRA cannot admit a request whose schedule ends past `u64::MAX` clock
  nanoseconds (~584 years after the clock's epoch); it refuses instead.
- While a clock is behind, `retry_after` is measured from the backwards
  reading and may be longer than necessary (never shorter).
- The crate cannot be published under its current package name
  (`ratelimit-rs` is taken on crates.io) — owner decision.
- MSRV 1.70 not verified on this host (toolchain not installed).
- No benchmarks exist; the README makes no performance claims.

## Acceptance checklist

| Item | Expected behaviour | Command / test | Result | Actual | Reference |
| --- | --- | --- | --- | --- | --- |
| Token bucket correctness | Matches an independent `u128` model on 300 random quotas × 400 steps | `cargo test --test properties token_bucket_and_gcra_match` | pass | identical decisions incl. `remaining`/`retry_after` | `tests/properties.rs` |
| GCRA ≡ token bucket | Identical decisions for monotonic clocks | `cargo test --test comparison bucket_and_gcra` and the model test above | pass | identical | `tests/comparison.rs` |
| Window correctness | Fixed/sliding match log-based reference models | `cargo test --test properties window_matches_the_reference_model` | pass | identical allow/deny | `tests/properties.rs` |
| `retry_after` exact | Waiting `retry_after` succeeds; 1 ns less fails, all four | `cargo test --test properties retry_after_is_exact` | pass | exact for 300 seeds each | `tests/properties.rs` |
| Backwards clock | Never mints permits | `cargo test --test properties survives_a_backwards_clock` | pass (failed before fix #2) | bounds hold | `src/{fixed,sliding}_window.rs` |
| Extreme values | No overflow/panic, never above conservation bound | `cargo test --test properties extreme` | pass (failed before fix #1/#3) | bounds hold | `src/gcra.rs`, `src/sliding_window.rs` |
| Concurrency | Exact totals under 8 threads for `Shared` and `Shared<Keyed>` | `cargo test --all-targets` (`concurrent*`, `shared_keyed*`) | pass | exact | `tests/*.rs` |
| README example output | Byte-identical to fixture | `cargo run --example compare` + `diff` | pass | identical | `examples/compare.rs` |
| Lint/docs | fmt, clippy `-D warnings`, rustdoc `-D warnings` clean | see table above | pass | clean | — |

## Classification proposal

**Partially Verified** — all documented behaviour is tested and passing
on macOS with fixes for two fail-open defects; not yet verified: the
audit branch's CI run (Linux/Windows/MSRV 1.70), which happens only
after the lead pushes. The package-name collision on crates.io needs an
owner decision before any publication.
