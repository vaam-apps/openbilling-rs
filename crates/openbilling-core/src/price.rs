//! The price book (ADR 0004): raw usage events, what rating answers, the versions of the price book, and the port
//! that loads, reads and rates, [`PriceBook`].
//!
//! The rules are PostgreSQL's (agent guide, rule 12): which version is in force at an instant, the price of a meter
//! in a currency for its dimensions, the rounding, the refusals of an unpriced meter or dimension, the checks of a
//! price list, the content hash that makes loading once-only, and the guard that no version is changed or
//! back-dated. This module names what goes in and out of them.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use crate::amount::Amount;
use crate::error::Error;
use crate::id::{CurrencyCode, DimensionKey, DimensionValue, EventId, MemberId, MeterId, OwnerId};
use crate::refusal::LedgerError;
use crate::store::LedgerStore;
use crate::time::Timestamp;
use crate::usage::IntakeReport;

/// A quantity of a meter's unit: a whole number, zero or more, up to `i64::MAX` (rule 3: integers only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quantity(i64);

impl Quantity {
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// The largest quantity, `i64::MAX`.
    pub const MAX: Self = Self(i64::MAX);

    /// A quantity of `units`.
    ///
    /// # Errors
    ///
    /// [`Error::QuantityOutOfRange`] for a negative number.
    pub const fn new(units: i64) -> Result<Self, Error> {
        if units < 0 {
            Err(Error::QuantityOutOfRange)
        } else {
            Ok(Self(units))
        }
    }

    /// The number of units.
    #[must_use]
    pub const fn units(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The dimensions of a raw event: each key at most once, in key order. A meter is priced by exactly its own
/// dimension keys; an event whose keys or values have no price is refused, never priced at a guess (ADR 0004).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Dimensions(BTreeMap<DimensionKey, DimensionValue>);

impl Dimensions {
    /// No dimension.
    #[must_use]
    pub const fn none() -> Self {
        Self(BTreeMap::new())
    }

    /// Sets `key` to `value`, replacing what it held.
    #[must_use]
    pub fn with(mut self, key: DimensionKey, value: DimensionValue) -> Self {
        self.0.insert(key, value);
        self
    }

    /// Each key and its value, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&DimensionKey, &DimensionValue)> {
        self.0.iter()
    }

    /// How many dimensions there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there is none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(DimensionKey, DimensionValue)> for Dimensions {
    fn from_iter<I: IntoIterator<Item = (DimensionKey, DimensionValue)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A raw usage event: a quantity of a meter, rated at intake by the price book version in force at its occurrence
/// time (ADR 0004). Once rated it goes through the same once-only intake and drain as a priced
/// [`UsageEvent`](crate::UsageEvent), and keeps its version.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RawEvent {
    /// The caller-given id. With the occurrence day, the once-only key.
    pub id: EventId,
    /// The owner whose accounts pay for it.
    pub owner: OwnerId,
    /// The currency it is rated and paid in.
    pub currency: CurrencyCode,
    /// The member who caused it.
    pub member: MemberId,
    /// What it measured.
    pub meter: MeterId,
    /// How many of the meter's unit.
    pub quantity: Quantity,
    /// What it is priced by, for example the model.
    pub dimensions: Dimensions,
    /// When it occurred; the version in force then rates it.
    pub occurred_at: Timestamp,
}

/// The id of a price book version, as the store gives it: the text of a UUID (lower-case hex, `8-4-4-4-12`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriceVersionId(String);

impl PriceVersionId {
    /// Checks that `value` is the text of a UUID and wraps it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVersionId`] for anything else.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        let shaped = value.len() == 36
            && value.bytes().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
            });
        if shaped {
            Ok(Self(value))
        } else {
            Err(Error::InvalidVersionId)
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PriceVersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What rating answered for one raw event.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rated {
    /// The amount, in micro-units of the event's currency: `quantity × price ÷ per`, rounded once, half to even.
    pub amount: Amount,
    /// The version that priced it: the one in force at the event's occurrence time.
    pub version: PriceVersionId,
}

/// What loading a price list answered.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Loaded {
    /// The version the list is, new or already there.
    pub version: PriceVersionId,
    /// When it takes effect.
    pub effective_from: Timestamp,
    /// The SHA-256 of the list's content, in lower-case hex: the once-only key of a version.
    pub content_hash: String,
    /// The same content was already loaded; nothing changed.
    pub replayed: bool,
}

/// A version of the price book, as the store shows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PriceVersionView {
    /// Its id.
    pub version: PriceVersionId,
    /// When it takes effect. It is in force until the next version takes effect.
    pub effective_from: Timestamp,
    /// The SHA-256 of its content, in lower-case hex.
    pub content_hash: String,
}

/// The price book: a versioned list of prices per meter, currency and dimensions, and the rating of raw events
/// against it (ADR 0004).
///
/// A version is loaded from a YAML price list, all or nothing, once per content; it never changes and is never
/// back-dated. A raw event is rated by the version in force at its occurrence time; an unpriced meter or dimension
/// is refused, never priced at a guess.
pub trait PriceBook: LedgerStore {
    /// Loads a YAML price list as a new version, or answers the version already loaded with the same content.
    /// A malformed list is refused naming its line and, where it can, its key.
    fn load_price_list(
        &self,
        yaml: &str,
    ) -> impl Future<Output = Result<Loaded, LedgerError>> + Send;

    /// Every version, oldest effective first.
    fn price_versions(
        &self,
    ) -> impl Future<Output = Result<Vec<PriceVersionView>, LedgerError>> + Send;

    /// What `event` costs: the amount and the version that prices it. Writes nothing.
    fn rate(&self, event: &RawEvent) -> impl Future<Output = Result<Rated, LedgerError>> + Send;

    /// Rates a batch of raw events and takes it into the queue, all or nothing, as
    /// [`LedgerStore::intake`] does for priced events.
    fn intake_raw(
        &self,
        events: &[RawEvent],
    ) -> impl Future<Output = Result<IntakeReport, LedgerError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quantity_is_a_whole_number_of_zero_or_more() {
        assert_eq!(Quantity::new(0), Ok(Quantity::ZERO));
        assert_eq!(Quantity::new(i64::MAX), Ok(Quantity::MAX));
        assert_eq!(Quantity::new(-1), Err(Error::QuantityOutOfRange));
        assert_eq!(Quantity::new(42).map(Quantity::units), Ok(42));
    }

    #[test]
    fn dimensions_keep_each_key_once_in_key_order() {
        let key = |k: &str| DimensionKey::new(k).expect("key");
        let value = |v: &str| DimensionValue::new(v).expect("value");
        let dims = Dimensions::none()
            .with(key("tier"), value("b"))
            .with(key("model"), value("x"))
            .with(key("tier"), value("c"));
        let keys: Vec<_> = dims.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(keys, [("model", "x"), ("tier", "c")]);
        assert_eq!(dims.len(), 2);
        assert!(Dimensions::none().is_empty());
    }

    #[test]
    fn a_version_id_is_the_text_of_a_uuid() {
        assert!(PriceVersionId::new("019ac5e0-7b1c-7d2e-8f00-0123456789ab").is_ok());
        for bad in [
            "",
            "019AC5E0-7B1C-7D2E-8F00-0123456789AB",
            "019ac5e07b1c7d2e8f000123456789ab",
            "019ac5e0-7b1c-7d2e-8f00-0123456789ag",
        ] {
            assert_eq!(
                PriceVersionId::new(bad),
                Err(Error::InvalidVersionId),
                "{bad}"
            );
        }
    }
}
