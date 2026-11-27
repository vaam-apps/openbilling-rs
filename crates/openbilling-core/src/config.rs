//! What the operator configures: the currencies, the account labels and their draw order, the order lots are drawn
//! in within an account, and the intake window (ADR 0001, point 8; ADR 0002).
//!
//! These are data. The rules that use them (the draw, the window, the checks of a configuration) are written once,
//! in PostgreSQL (agent guide, rule 12).

use std::fmt;

use crate::error::Error;
use crate::id::{AccountLabel, CurrencyCode};

/// Where credit came from (ADR 0002). Closed on purpose: a new source must fail to compile wherever sources are
/// matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CreditSource {
    /// Money paid, keyed by the external payment id. The only source a refund or a chargeback can reverse.
    Payment,
    /// Promotional credit, keyed by a caller-given grant id, usually with an expiry.
    Promotion,
    /// An operator's correction, keyed by a caller-given id. Positive opens a lot; negative takes credit away.
    Adjustment,
}

impl CreditSource {
    /// Every source, in declaration order.
    pub const ALL: [Self; 3] = [Self::Payment, Self::Promotion, Self::Adjustment];

    /// The source's stable name, as the database stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Payment => "payment",
            Self::Promotion => "promotion",
            Self::Adjustment => "adjustment",
        }
    }

    /// The kind of lot a positive credit of this source opens.
    #[must_use]
    pub const fn lot_kind(self) -> LotKind {
        match self {
            Self::Payment => LotKind::Payment,
            Self::Promotion => LotKind::Promotion,
            Self::Adjustment => LotKind::Adjustment,
        }
    }

    /// The source of a stored name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == name)
    }
}

impl fmt::Display for CreditSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a payment is taken back. Both are capped, together, at the payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReversalKind {
    /// The operator gave the money back.
    Refund,
    /// The payer's bank took it back.
    Chargeback,
}

impl ReversalKind {
    /// The kind's stable name, as the database stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refund => "refund",
            Self::Chargeback => "chargeback",
        }
    }
}

/// A kind of lot: a parcel of credit in one account, drawn by the debit. Closed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LotKind {
    /// The month's allowance: granted once per account and UTC month, spendable during that month only.
    Allowance,
    /// Opened by a promotion credit.
    Promotion,
    /// Opened by a positive adjustment.
    Adjustment,
    /// Opened by a payment.
    Payment,
}

impl LotKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Allowance,
        Self::Promotion,
        Self::Adjustment,
        Self::Payment,
    ];

    /// The kind's stable name, as the database stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowance => "allowance",
            Self::Promotion => "promotion",
            Self::Adjustment => "adjustment",
            Self::Payment => "payment",
        }
    }

    /// The kind of a stored name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

impl fmt::Display for LotKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The order lots of one kind are drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WithinKind {
    /// The lot that expires first is drawn first; a lot with no expiry comes after every lot with one. Ties: oldest
    /// first.
    SoonestExpiryFirst,
    /// The oldest lot is drawn first.
    OldestFirst,
}

/// The order the debit draws lots in within one account: each [`LotKind`] once, first drawn first. The store refuses
/// an order that leaves a kind out or names one twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LotOrder(pub Vec<(LotKind, WithinKind)>);

impl Default for LotOrder {
    /// The example order, which the first migration also seeds: the allowance, then promotions by soonest expiry,
    /// then adjustments and payments oldest first.
    fn default() -> Self {
        Self(vec![
            (LotKind::Allowance, WithinKind::SoonestExpiryFirst),
            (LotKind::Promotion, WithinKind::SoonestExpiryFirst),
            (LotKind::Adjustment, WithinKind::OldestFirst),
            (LotKind::Payment, WithinKind::OldestFirst),
        ])
    }
}

/// The ledger's configuration (ADR 0002).
///
/// `currencies` is the set of currencies the operator names. `labels` lists the account labels in the order the
/// debit draws an owner's accounts of one currency: the first label first. A configuration replaces the previous one.
/// The store refuses one that names a currency or a label twice, has none of either, or leaves out a currency or a
/// label already configured, since accounts may name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerConfig {
    /// The currencies.
    pub currencies: Vec<CurrencyCode>,
    /// The labels, first drawn first.
    pub labels: Vec<AccountLabel>,
    /// The order of lots within an account.
    pub lot_order: LotOrder,
}

/// The intake window: an event is accepted only if it occurred no earlier than the start of the UTC day `days − 1`
/// days ago and no later than now plus `skew` (ADR 0001, point 8). It is also how long a day's partition is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    days: u16,
    skew_micros: i64,
}

impl Window {
    /// A window of `days` days and `skew_micros` microseconds of clock skew allowed ahead.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidWindow`] for zero days or a negative skew.
    pub const fn new(days: u16, skew_micros: i64) -> Result<Self, Error> {
        if days == 0 || skew_micros < 0 {
            Err(Error::InvalidWindow)
        } else {
            Ok(Self { days, skew_micros })
        }
    }

    /// The number of days.
    #[must_use]
    pub const fn days(self) -> u16 {
        self.days
    }

    /// The skew allowance, in microseconds.
    #[must_use]
    pub const fn skew_micros(self) -> i64 {
        self.skew_micros
    }
}

impl Default for Window {
    /// Seven days and five minutes (ADR 0001, point 8).
    fn default() -> Self {
        Self {
            days: 7,
            skew_micros: 5 * 60 * 1_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        let sources: Vec<_> = CreditSource::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(sources, ["payment", "promotion", "adjustment"]);
        let kinds: Vec<_> = LotKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(kinds, ["allowance", "promotion", "adjustment", "payment"]);
        for kind in LotKind::ALL {
            assert_eq!(LotKind::parse(kind.as_str()), Some(kind));
        }
        for source in CreditSource::ALL {
            assert_eq!(CreditSource::parse(source.as_str()), Some(source));
            assert_eq!(source.lot_kind().as_str(), source.as_str());
        }
        assert_eq!(ReversalKind::Refund.as_str(), "refund");
        assert_eq!(ReversalKind::Chargeback.as_str(), "chargeback");
    }

    #[test]
    fn the_default_lot_order_is_the_example_and_names_every_kind_once() {
        let kinds: Vec<_> = LotOrder::default().0.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, LotKind::ALL);
    }

    #[test]
    fn a_window_has_at_least_one_day_and_no_negative_skew() {
        let window = Window::default();
        assert_eq!((window.days(), window.skew_micros()), (7, 300_000_000));
        assert_eq!(Window::new(0, 0), Err(Error::InvalidWindow));
        assert_eq!(Window::new(1, -1), Err(Error::InvalidWindow));
        assert!(Window::new(1, 0).is_ok());
    }
}
