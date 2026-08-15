//! [`Timestamp`]: a point in time as the core sees it.
//!
//! The core has no clock. Every function that depends on time takes `now`
//! as an argument, and every trace event that records a decision records
//! the `now` it was made with — that is what makes decisions replayable.

use std::fmt;

/// Seconds since the Unix epoch (`1970-01-01T00:00:00Z`), signed.
///
/// `i64` matches Cedar's `long` and the usual `timestamp()` accessors of
/// date-time libraries. Negative values are meaningless in practice but
/// harmless: only comparison is ever performed, never arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Wraps a number of seconds since the Unix epoch.
    pub const fn from_unix_secs(secs: i64) -> Self {
        Timestamp(secs)
    }

    /// The number of seconds since the Unix epoch.
    pub const fn unix_secs(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_by_seconds() {
        let a = Timestamp::from_unix_secs(1);
        let b = Timestamp::from_unix_secs(2);
        assert!(a < b);
        assert_eq!(b.unix_secs(), 2);
        assert_eq!(a.to_string(), "1");
        assert!(Timestamp::from_unix_secs(-1) < Timestamp::from_unix_secs(0));
    }
}
