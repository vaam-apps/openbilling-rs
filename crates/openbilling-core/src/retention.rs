//! Partition retention (ADR 0001, point 10, amended 2026-11-28): the port that drops the usage partitions nobody can
//! replay into any more, and what it is told and answers.
//!
//! The rule (which day is old enough, which partition holds an undecided event) is SQL, in `openbilling-pg`'s
//! migrations. This module names what goes in and out of it.

use std::future::Future;

use crate::{Error, LedgerError, UtcDay};

/// How long a day's partition outlives the intake window before it is dropped, in whole days (the owner's decision of
/// 2026-11-28: "drop after W plus a margin, default 90 days, configurable"). A business value, so the deployer sets
/// it; [`RetentionMargin::default`] is the documented default, 90 days.
///
/// At least one day (the grace day of ADR 0001, point 10: a statement that started before midnight, and any drift
/// between clocks, never meets a dropped partition) and at most [`RetentionMargin::MAX_DAYS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RetentionMargin(u16);

impl RetentionMargin {
    /// The longest margin: ten years.
    pub const MAX_DAYS: u16 = 3650;

    /// A margin of `days` days.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRetentionMargin`] for zero days or more than [`Self::MAX_DAYS`].
    pub const fn new(days: u16) -> Result<Self, Error> {
        if days == 0 || days > Self::MAX_DAYS {
            Err(Error::InvalidRetentionMargin)
        } else {
            Ok(Self(days))
        }
    }

    /// The number of days.
    #[must_use]
    pub const fn days(self) -> u16 {
        self.0
    }
}

impl Default for RetentionMargin {
    /// Ninety days.
    fn default() -> Self {
        Self(90)
    }
}

/// What one run of the retention job did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionReport {
    /// The days whose partitions were detached and dropped, oldest first.
    pub dropped: Vec<UtcDay>,
    /// Another run held the job's lock, so this one did nothing.
    pub skipped: bool,
}

/// Drops the usage partitions older than the intake window plus a margin (ADR 0001, point 10). An extension of the
/// storage port: the store that keeps the queue knows its partitions.
///
/// A run is safe to repeat and safe to run on several instances at once: one takes the lock and the others report
/// [`RetentionReport::skipped`]. A partition inside the window plus the margin, or holding an event that is still
/// undecided, is never dropped. Outcomes are kept for good in a table of their own, so a replay is still refused
/// after its partition is gone: by the window while it is recent, by the outcome after.
pub trait PartitionRetention: Send + Sync {
    /// Detaches and drops every partition whose whole day is older than the window plus `margin`, and answers the
    /// days it dropped.
    fn drop_expired_partitions(
        &self,
        margin: RetentionMargin,
    ) -> impl Future<Output = Result<RetentionReport, LedgerError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_margin_is_a_day_to_ten_years_and_defaults_to_ninety_days() {
        assert_eq!(RetentionMargin::default().days(), 90);
        assert_eq!(RetentionMargin::new(0), Err(Error::InvalidRetentionMargin));
        assert_eq!(RetentionMargin::new(1).unwrap().days(), 1);
        assert_eq!(RetentionMargin::new(3650).unwrap().days(), 3650);
        assert_eq!(
            RetentionMargin::new(3651),
            Err(Error::InvalidRetentionMargin)
        );
    }
}
