//! Money as an integer number of micro-units of one currency.

use std::fmt;
use std::ops::Neg;

use crate::error::Error;

/// An amount of money in **micro-units** of its currency: one unit is 10^-6 of the currency's major unit (ADR 0001,
/// point 20, amended 2026-11-27; agent guide, rule 3). Never a float.
///
/// Micro-units, not the currency's minor unit, because one usage event is often worth less than one minor unit. An
/// `i64` of micro-units still holds about 9.2 × 10^12 major units. There is no conversion between currencies: each
/// account carries one currency, the operator names the set of currencies, and an amount is only ever added to
/// amounts of the same currency.
///
/// The value is an `i64`, PostgreSQL's `bigint`. Its range is symmetric, `-(2^63 - 1)` to `2^63 - 1`: `i64::MIN` is
/// refused, so every amount can be negated, and the balancing entry of any entry exists. Arithmetic is checked and
/// never wraps.
///
/// An `Amount` does not carry its currency. The ledger keeps entries apart by currency (ADR 0001, point 21), and the
/// type that pairs the two comes with the ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount(i64);

impl Amount {
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// The largest amount, `2^63 - 1` micro-units.
    pub const MAX: Self = Self(i64::MAX);
    /// The smallest amount, `-(2^63 - 1)` micro-units: the negation of [`Amount::MAX`].
    pub const MIN: Self = Self(-i64::MAX);

    /// An amount of `micros` micro-units.
    ///
    /// # Errors
    ///
    /// [`Error::AmountOutOfRange`] for `i64::MIN`, which has no negation.
    pub const fn from_micros(micros: i64) -> Result<Self, Error> {
        if micros == i64::MIN {
            Err(Error::AmountOutOfRange)
        } else {
            Ok(Self(micros))
        }
    }

    /// The number of micro-units.
    #[must_use]
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// `self + other`, or `None` when the sum leaves the range.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) if sum != i64::MIN => Some(Self(sum)),
            _ => None,
        }
    }

    /// `self - other`, or `None` when the difference leaves the range.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(difference) if difference != i64::MIN => Some(Self(difference)),
            _ => None,
        }
    }

    /// Whether this is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether this is less than zero.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Whether this is more than zero.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }
}

impl Neg for Amount {
    type Output = Self;

    /// Never overflows: the range is symmetric.
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl TryFrom<i64> for Amount {
    type Error = Error;

    fn try_from(micros: i64) -> Result<Self, Self::Error> {
        Self::from_micros(micros)
    }
}

impl From<Amount> for i64 {
    fn from(amount: Amount) -> Self {
        amount.0
    }
}

impl fmt::Display for Amount {
    /// The number of micro-units, as a plain integer. Formatting for people needs the currency, which this type
    /// does not carry.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(micros: i64) -> Amount {
        Amount::from_micros(micros).expect("in range")
    }

    #[test]
    fn i64_min_is_refused_and_everything_else_is_kept() {
        assert_eq!(Amount::from_micros(i64::MIN), Err(Error::AmountOutOfRange));
        assert_eq!(Amount::try_from(i64::MIN), Err(Error::AmountOutOfRange));
        for micros in [-i64::MAX, -1, 0, 1, i64::MAX] {
            assert_eq!(amount(micros).micros(), micros);
            assert_eq!(i64::from(amount(micros)), micros);
        }
    }

    #[test]
    fn every_amount_negates() {
        assert_eq!(-Amount::MAX, Amount::MIN);
        assert_eq!(-Amount::MIN, Amount::MAX);
        assert_eq!(-Amount::ZERO, Amount::ZERO);
        assert_eq!(-amount(250), amount(-250));
    }

    #[test]
    fn an_entry_and_its_negation_sum_to_zero() {
        for micros in [-i64::MAX, -7, 0, 7, i64::MAX] {
            let entry = amount(micros);
            assert_eq!(entry.checked_add(-entry), Some(Amount::ZERO));
        }
    }

    #[test]
    fn arithmetic_is_checked_and_never_reaches_i64_min() {
        assert_eq!(amount(2).checked_add(amount(3)), Some(amount(5)));
        assert_eq!(amount(2).checked_sub(amount(3)), Some(amount(-1)));
        assert_eq!(Amount::MAX.checked_add(amount(1)), None);
        // -(2^63 - 1) - 1 is i64::MIN: representable as an i64, refused as an Amount.
        assert_eq!(Amount::MIN.checked_sub(amount(1)), None);
        assert_eq!(Amount::MIN.checked_add(amount(-1)), None);
        assert_eq!(Amount::MAX.checked_sub(Amount::MIN), None);
    }

    #[test]
    fn signs() {
        assert!(Amount::ZERO.is_zero());
        assert!(!Amount::ZERO.is_negative() && !Amount::ZERO.is_positive());
        assert!(amount(-1).is_negative());
        assert!(amount(1).is_positive());
    }

    #[test]
    fn displays_micro_units() {
        assert_eq!(amount(-1050).to_string(), "-1050");
        assert_eq!(Amount::default(), Amount::ZERO);
    }
}
