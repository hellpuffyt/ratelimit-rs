# Contributing

## Setup

You need a stable Rust toolchain (1.70 or newer) with `rustfmt` and
`clippy`:

```
rustup component add rustfmt clippy
```

Clone and build:

```
git clone https://github.com/hellpuffyt/ratelimit-rs
cd ratelimit-rs
cargo build --all-targets
```

## Before opening a PR

Run the same checks CI runs, in this order:

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --doc
cargo run --example compare
```

All four must pass with no warnings. `cargo clippy -- -D warnings` means
what it says: a new lint is a build failure, not a follow-up.

## Ground rules specific to this project

- **No `std::time::Instant::now()` (or `SystemTime::now()`) inside a
  limiter.** Every limiter reads time through the `Clock` trait. This is
  the one rule the whole test strategy depends on — it's what lets tests
  advance a `ManualClock` by an exact `Duration::from_nanos(1)` instead of
  sleeping and hoping. A PR that adds a direct call to real wall-clock time
  inside `src/` (outside `SystemClock` itself) will be asked to change it.

- **No new runtime dependencies without a very good reason.** The crate is
  `std`-only today, and the README makes that a selling point. A dev
  dependency for testing is a different, much lower bar — feel free to
  propose one if it earns its keep.

- **A new algorithm-affecting behaviour needs a test that would fail
  without it, not just a passing test that happens to exercise the code
  path.** If you fix an off-by-one at a window boundary, the test should
  assert the boundary nanosecond both ways (one nanosecond before, exactly
  on, one nanosecond after), the way the existing tests do.

- **If a change affects how the four algorithms compare to each other**
  (burst size, recovery shape, worst case over a span), update
  `tests/comparison.rs` and the table in `src/lib.rs`'s doc comment
  together — that test exists specifically so the two can't drift apart.

- **Public items need doc comments**, checked by `#![warn(missing_docs)]`.
  A doctest that demonstrates real usage is preferred over prose alone
  where it's not too verbose.

- Keep `unsafe` out of it — `#![forbid(unsafe_code)]` is a crate-level
  attribute, not a suggestion.

## Reporting a bug

A rate limiter bug report is much more useful with a minimal
`ManualClock` reproduction than with a description of production
behaviour — see any existing test in `tests/` for the shape. If you can
turn what you're seeing into a five-line test that fails, that test is
most of the fix already.
