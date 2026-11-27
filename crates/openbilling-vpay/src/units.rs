//! vpay's amounts are integers of a currency's minor unit; openbilling's are micro-units (10^-6 of the major
//! unit). The scale is a power of ten per currency, and no currency is ever converted into another.
//!
//! *Verified 2026-11-27*, vpay `8b56831`: `amount` is "a whole number of the currency's minor units", positive and at
//! most 2^53 − 1 (`parse_amount` and `MAX_AMOUNT` in `backends/crates/vpay-api/src/v1/payment_intents.rs`); each
//! currency's exponent is a property of the currency (`Currency::exponent` in `backends/crates/vpay-core/src/money.rs`).

use openbilling_core::{Amount, PaymentRefusal};

/// The largest amount vpay accepts, in minor units: 2^53 − 1.
pub const MAX_MINOR: i64 = (1_i64 << 53) - 1;

/// An amount in a currency's minor units, as vpay writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinorUnits(pub i64);

fn scale(exponent: u32) -> Option<i64> {
    6_u32
        .checked_sub(exponent)
        .and_then(|power| 10_i64.checked_pow(power))
}

impl MinorUnits {
    /// The minor units of `amount` micro-units, for a currency of `exponent` (0 to 6).
    ///
    /// # Errors
    ///
    /// [`PaymentRefusal::InexactAmount`] when the micro-units are not a whole number of minor units, and
    /// [`PaymentRefusal::InvalidRequest`] when the amount is not positive or passes what vpay accepts.
    pub fn from_micros(amount: Amount, exponent: u32) -> Result<Self, PaymentRefusal> {
        let scale = scale(exponent).ok_or(PaymentRefusal::InvalidRequest)?;
        let micros = amount.micros();
        if micros <= 0 {
            return Err(PaymentRefusal::InvalidRequest);
        }
        if micros % scale != 0 {
            return Err(PaymentRefusal::InexactAmount);
        }
        let minor = micros / scale;
        if minor > MAX_MINOR {
            return Err(PaymentRefusal::InvalidRequest);
        }
        Ok(Self(minor))
    }

    /// The micro-units of these minor units, for a currency of `exponent`; `None` when it does not fit.
    #[must_use]
    pub fn to_micros(self, exponent: u32) -> Option<Amount> {
        let micros = self.0.checked_mul(scale(exponent)?)?;
        Amount::from_micros(micros).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn micros(v: i64) -> Amount {
        Amount::from_micros(v).expect("an amount")
    }

    #[test]
    fn cents_and_whole_units_scale_exactly() {
        assert_eq!(
            MinorUnits::from_micros(micros(12_340_000), 2),
            Ok(MinorUnits(1_234))
        );
        assert_eq!(
            MinorUnits::from_micros(micros(5_000_000_000), 0),
            Ok(MinorUnits(5_000))
        );
        assert_eq!(MinorUnits::from_micros(micros(7), 6), Ok(MinorUnits(7)));
        assert_eq!(MinorUnits(1_234).to_micros(2), Some(micros(12_340_000)));
        assert_eq!(MinorUnits(5_000).to_micros(0), Some(micros(5_000_000_000)));
    }

    #[test]
    fn what_vpay_cannot_hold_is_refused() {
        assert_eq!(
            MinorUnits::from_micros(micros(12_345), 2),
            Err(PaymentRefusal::InexactAmount)
        );
        assert_eq!(
            MinorUnits::from_micros(Amount::ZERO, 2),
            Err(PaymentRefusal::InvalidRequest)
        );
        assert_eq!(
            MinorUnits::from_micros(micros(-10_000), 2),
            Err(PaymentRefusal::InvalidRequest)
        );
        assert_eq!(
            MinorUnits::from_micros(micros(MAX_MINOR + 1), 6),
            Err(PaymentRefusal::InvalidRequest)
        );
        assert_eq!(MinorUnits(i64::MAX).to_micros(0), None);
        assert_eq!(MinorUnits(1).to_micros(7), None);
    }
}
