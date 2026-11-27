//! CrateStack's `DateTime` (RFC 3339 on the wire, `chrono` in Rust) and the ledger's [`Timestamp`].

use cratestack::CratestackError;
use cratestack::chrono::{DateTime, Utc};
use openbilling_core::Timestamp;

/// The ledger's instant of a CrateStack `DateTime`.
///
/// # Errors
///
/// `422` naming `field` for an instant before 1970 or after 9999.
pub fn timestamp(at: &DateTime<Utc>, field: &str) -> Result<Timestamp, CratestackError> {
    Timestamp::from_unix_micros(at.timestamp_micros())
        .map_err(|e| crate::errors::invalid(field, &e))
}

/// The CrateStack `DateTime` of a ledger instant.
#[must_use]
pub fn date_time(at: Timestamp) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(at.unix_micros())
}
