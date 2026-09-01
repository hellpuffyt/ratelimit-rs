//! The single error type, for the single fallible thing in this crate.
//!
//! Acquiring a permit cannot fail — it is allowed or it is not, and that
//! is a [`Decision`](crate::Decision), not an error. The only way to
//! misuse this library is to describe a rate that does not exist, and
//! that is caught once, at construction, by [`Quota`](crate::Quota).

use std::error::Error;
use std::fmt;

/// Why a [`Quota`](crate::Quota) could not be built.
///
/// Every variant is a configuration mistake that would otherwise turn
/// into a limiter that silently does the wrong thing — permits nothing,
/// permits everything, or overflows its own arithmetic — so all of them
/// are rejected at construction rather than tolerated at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuotaError {
    /// The permit count was zero, which describes no rate at all.
    ZeroCount,
    /// The period was zero, so the rate would be infinite.
    ZeroPeriod,
    /// The burst allowance was zero, so no permit could ever be issued.
    ZeroBurst,
    /// `period / count` truncated to zero nanoseconds: more than one
    /// permit per nanosecond, which has no representable spacing.
    RateTooHigh,
    /// `burst * (period / count)` overflows [`u64`] nanoseconds — a
    /// bucket that would take longer than 584 years to fill.
    BurstTooLarge,
}

impl fmt::Display for QuotaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ZeroCount => "permit count must be greater than zero",
            Self::ZeroPeriod => "period must be greater than zero",
            Self::ZeroBurst => "burst must be greater than zero",
            Self::RateTooHigh => {
                "rate exceeds one permit per nanosecond, which has no representable spacing"
            }
            Self::BurstTooLarge => "burst times the emission interval overflows u64 nanoseconds",
        };
        f.write_str(message)
    }
}

impl Error for QuotaError {}
