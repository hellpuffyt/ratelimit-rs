//! Four rate limiters, one quota type, an injectable clock, and no
//! runtime dependencies.
//!
//! ```
//! use std::time::Duration;
//! use ratelimit::{Decision, Quota, RateLimiter, TokenBucket};
//!
//! // 100 requests per minute, but up to 10 may arrive at once.
//! let quota = Quota::per_minute(100)?.with_burst(10)?;
//! let mut limiter = TokenBucket::new(quota);
//!
//! match limiter.try_acquire() {
//!     Decision::Allow { remaining } => println!("served; {remaining} left"),
//!     Decision::Deny { retry_after } => println!("429; retry in {retry_after:?}"),
//!     Decision::Exceeds { capacity } => println!("400; the most we ever allow is {capacity}"),
//! }
//! # Ok::<(), ratelimit::QuotaError>(())
//! ```
//!
//! # Choosing an algorithm
//!
//! All four enforce the same long-run rate. They differ in what they let
//! through *in the short run*, which is the only thing that matters when
//! choosing between them.
//!
//! | | Burst at one instant | Recovery after exhaustion | Worst case in any window-length span | State |
//! | --- | --- | --- | --- | --- |
//! | [`TokenBucket`] | `burst`, tunable | one permit every `period / count` | `burst + count` | 16 bytes |
//! | [`Gcra`] | `burst`, tunable | one permit every `period / count` | `burst + count` | 8 bytes |
//! | [`SlidingWindow`] | `count` | released continuously as the old window ages out | under `2 * count` | 12 bytes |
//! | [`FixedWindow`] | `count` | nothing, then all of it at the boundary | `2 * count` | 12 bytes |
//!
//! Every number in that table is asserted by a test rather than
//! reasoned about in prose — see `tests/comparison.rs`, which drives all
//! four limiters through the same schedules and pins the differences.
//!
//! In short:
//!
//! * **[`TokenBucket`]** — the default. Absorbs the bursts real clients
//!   produce while holding the average, and `burst` is a dial you can
//!   set independently of the rate.
//! * **[`Gcra`]** — the same decisions in half the state, and it knows
//!   *when* the next permit falls due
//!   ([`Gcra::next_permit_in`]), which is what a scheduler wants. Its
//!   conventional configuration, `with_burst(1)`, is a strict pacer.
//!   That it permits exactly what [`TokenBucket`] permits is not a claim
//!   here, it is a test.
//! * **[`SlidingWindow`]** — when the limit is a promise about a named
//!   window ("600 per minute") and you want it to hold for *every*
//!   minute-long span. Costs an approximation; see the type docs for
//!   which direction it errs in.
//! * **[`FixedWindow`]** — when the boundary is part of the contract: a
//!   billing period, a daily allowance. Cheap, easy to explain, and lets
//!   a client take `2 * count` across a boundary, which is fine when the
//!   boundary is real and unacceptable when it is not.
//!
//! # Time is injected
//!
//! No limiter calls [`std::time::Instant::now`]. They read a [`Clock`],
//! which in production is [`SystemClock`] and in tests is
//! [`ManualClock`]. That is what makes the behaviour above assertable:
//! a test can stand one nanosecond either side of a window boundary,
//! exactly, and the whole suite runs without sleeping.
//!
//! The same mechanism is available to you. If your service's behaviour
//! under a rate limit is worth testing — the retry path, the 429
//! handler, the queue that backs up — drive your limiter from a
//! [`ManualClock`] and test it in microseconds.
//!
//! # Shared and per-key use
//!
//! Limiters take `&mut self`, so sharing is explicit: wrap one in
//! [`Shared`] for `&self` access from several threads, and use [`Keyed`]
//! for a limiter per client. They compose —
//! `Shared<Keyed<IpAddr, Gcra>>` is a per-IP limiter usable from a thread
//! pool.
//!
//! # No dependencies
//!
//! The crate is `std` only, with no optional features and nothing to
//! audit. Rate limiting sits in front of everything else a service does,
//! so it is a poor place to inherit a dependency tree.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rustdoc::broken_intra_doc_links)]

mod clock;
mod decision;
mod error;
mod fixed_window;
mod gcra;
mod keyed;
mod quota;
mod shared;
mod sliding_window;
mod token_bucket;

pub use crate::clock::{Clock, ManualClock, SystemClock};
pub use crate::decision::{Decision, RateLimiter};
pub use crate::error::QuotaError;
pub use crate::fixed_window::FixedWindow;
pub use crate::gcra::Gcra;
pub use crate::keyed::{FromQuota, Keyed};
pub use crate::quota::Quota;
pub use crate::shared::Shared;
pub use crate::sliding_window::SlidingWindow;
pub use crate::token_bucket::TokenBucket;
