//! Caller-given ids and the names the operator configures.
//!
//! An id is chosen by the caller, so the service must count it once whatever the caller sends (ADR 0001, point 1).
//! Ids follow one rule: 1 to [`MAX_ID_LEN`] bytes of printable ASCII, `!` (0x21) to `~` (0x7E). No whitespace, no
//! control character and no non-ASCII text, so two ids that look the same are the same bytes, and an id can be
//! written to a log without escaping. An [`OwnerId`] may be longer, up to [`MAX_OWNER_LEN`] bytes, because it is the
//! subject an identity provider gives, and OpenID Connect allows 255 ASCII characters for one.
//!
//! The operator's names are narrower: a [`CurrencyCode`] is 1 to 16 bytes of `A`–`Z` and `0`–`9`, an
//! [`AccountLabel`] 1 to 64 bytes of `a`–`z`, `0`–`9`, `_` and `-` (ADR 0002).
//!
//! The price book's names (ADR 0004) are as narrow: a [`MeterId`] and a [`DimensionKey`] are 1 to 64 bytes of `a`–`z`,
//! `0`–`9`, `.`, `_` and `-` (`tokens.input`, `model`); a [`DimensionValue`] follows the rule of ids, up to
//! [`MAX_ID_LEN`] bytes.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, IdKind, IdProblem};

/// The longest id, in bytes.
pub const MAX_ID_LEN: usize = 128;

/// The longest [`OwnerId`], in bytes.
pub const MAX_OWNER_LEN: usize = 255;

/// The longest [`CurrencyCode`], in bytes.
pub const MAX_CURRENCY_LEN: usize = 16;

/// The longest [`AccountLabel`], in bytes.
pub const MAX_LABEL_LEN: usize = 64;

/// The longest [`MeterId`] or [`DimensionKey`], in bytes.
pub const MAX_NAME_LEN: usize = 64;

fn printable(b: u8) -> bool {
    b.is_ascii_graphic()
}

fn currency_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn label_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

fn name_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-'
}

fn check(kind: IdKind, value: &str, max: usize, allowed: fn(u8) -> bool) -> Result<(), Error> {
    let problem = if value.is_empty() {
        Some(IdProblem::Empty)
    } else if value.len() > max {
        Some(IdProblem::TooLong)
    } else if !value.bytes().all(allowed) {
        Some(IdProblem::ForbiddenCharacter)
    } else {
        None
    };
    match problem {
        Some(problem) => Err(Error::InvalidId { kind, problem }),
        None => Ok(()),
    }
}

macro_rules! caller_id {
    ($(#[$doc:meta])* $name:ident, $kind:expr, $max:expr, $allowed:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Checks `value` against the rules of its kind (see the module) and wraps it.
            ///
            /// # Errors
            ///
            /// [`Error::InvalidId`] when `value` is empty, too long, or holds a byte its kind does not allow.
            pub fn new(value: impl Into<String>) -> Result<Self, Error> {
                let value = value.into();
                check($kind, &value, $max, $allowed)?;
                Ok(Self(value))
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The id as an owned string.
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

caller_id!(
    /// The caller-given id of a usage event. With its occurrence day it is the event's once-only key
    /// (ADR 0001, point 7).
    EventId,
    IdKind::Event,
    MAX_ID_LEN,
    printable
);

caller_id!(
    /// The opaque id of an account: one currency and one label of one owner (ADR 0002). Given by whoever opens it,
    /// so opening it is once-only.
    AccountId,
    IdKind::Account,
    MAX_ID_LEN,
    printable
);

caller_id!(
    /// The external subject that holds accounts, for example the `sub` an identity provider gives (ADR 0002).
    OwnerId,
    IdKind::Owner,
    MAX_OWNER_LEN,
    printable
);

caller_id!(
    /// The member of an owner who caused a usage event, for a member cap. For a person's own owner it is the person.
    MemberId,
    IdKind::Member,
    MAX_ID_LEN,
    printable
);

caller_id!(
    /// The caller-given id of a credit or a reversal: an external payment id, a grant id, an adjustment id, a refund
    /// or a chargeback id. Once-only within its source or kind (ADR 0002).
    CreditId,
    IdKind::Credit,
    MAX_ID_LEN,
    printable
);

caller_id!(
    /// A currency the operator names, for example `EUR` or `CREDITS`: 1 to 16 bytes of `A`–`Z` and `0`–`9`. The
    /// ledger never converts between currencies.
    CurrencyCode,
    IdKind::Currency,
    MAX_CURRENCY_LEN,
    currency_byte
);

caller_id!(
    /// The label of an account within its owner and currency, for example `main` or `promo`: 1 to 64 bytes of
    /// `a`–`z`, `0`–`9`, `_` and `-`. The operator configures the labels and the order the debit draws them in.
    AccountLabel,
    IdKind::Label,
    MAX_LABEL_LEN,
    label_byte
);

caller_id!(
    /// A meter of the price book, for example `tokens.input` or `compute.seconds`: 1 to 64 bytes of `a`–`z`, `0`–`9`,
    /// `.`, `_` and `-` (ADR 0004). A raw usage event names one, and a quantity of its unit.
    MeterId,
    IdKind::Meter,
    MAX_NAME_LEN,
    name_byte
);

caller_id!(
    /// The key of a dimension a meter is priced by, for example `model`: the alphabet of a [`MeterId`] (ADR 0004).
    DimensionKey,
    IdKind::DimensionKey,
    MAX_NAME_LEN,
    name_byte
);

caller_id!(
    /// The value of a dimension, for example `example-small`: the rule of ids, printable ASCII up to [`MAX_ID_LEN`]
    /// bytes (ADR 0004).
    DimensionValue,
    IdKind::DimensionValue,
    MAX_ID_LEN,
    printable
);

#[cfg(test)]
mod tests {
    use super::*;

    fn problem_of(value: &str) -> Option<IdProblem> {
        match EventId::new(value) {
            Ok(_) => None,
            Err(Error::InvalidId { problem, .. }) => Some(problem),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn accepts_printable_ascii_up_to_the_limit() {
        for value in [
            "a",
            "evt_01J9Z8",
            "a-b.c:d/e@f",
            "~!",
            &"x".repeat(MAX_ID_LEN),
        ] {
            let id = EventId::new(value).expect("valid id");
            assert_eq!(id.as_str(), value);
            assert_eq!(id.to_string(), value);
        }
    }

    #[test]
    fn refuses_an_empty_id() {
        assert_eq!(problem_of(""), Some(IdProblem::Empty));
    }

    #[test]
    fn refuses_an_id_one_byte_too_long() {
        assert_eq!(
            problem_of(&"x".repeat(MAX_ID_LEN + 1)),
            Some(IdProblem::TooLong)
        );
    }

    #[test]
    fn refuses_whitespace_control_and_non_ascii() {
        for value in [
            "a b",
            " a",
            "a\t",
            "a\n",
            "a\u{0}",
            "a\u{7f}",
            "caf\u{e9}",
            "a\u{202e}b",
        ] {
            assert_eq!(
                problem_of(value),
                Some(IdProblem::ForbiddenCharacter),
                "{value:?}"
            );
        }
    }

    #[test]
    fn length_is_counted_in_bytes() {
        // 64 two-byte characters are 128 bytes: the length check passes and the ASCII check refuses.
        assert_eq!(
            problem_of(&"\u{e9}".repeat(64)),
            Some(IdProblem::ForbiddenCharacter)
        );
        assert_eq!(problem_of(&"\u{e9}".repeat(65)), Some(IdProblem::TooLong));
    }

    #[test]
    fn the_error_names_the_kind_of_id() {
        assert_eq!(
            AccountId::new(""),
            Err(Error::InvalidId {
                kind: IdKind::Account,
                problem: IdProblem::Empty
            })
        );
        assert_eq!(
            EventId::new(""),
            Err(Error::InvalidId {
                kind: IdKind::Event,
                problem: IdProblem::Empty
            })
        );
    }

    #[test]
    fn the_error_never_holds_the_value() {
        let error = AccountId::new("secret value").expect_err("whitespace is refused");
        assert!(!error.to_string().contains("secret"));
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn an_owner_may_be_as_long_as_an_oidc_subject() {
        assert!(OwnerId::new("s".repeat(MAX_OWNER_LEN)).is_ok());
        assert_eq!(
            OwnerId::new("s".repeat(MAX_OWNER_LEN + 1)),
            Err(Error::InvalidId {
                kind: IdKind::Owner,
                problem: IdProblem::TooLong
            })
        );
        assert!(EventId::new("e".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn currencies_and_labels_have_narrow_alphabets() {
        for good in ["EUR", "USD", "CREDITS", "X1"] {
            assert!(CurrencyCode::new(good).is_ok(), "{good}");
        }
        for bad in ["eur", "E-U", "E U", "\u{c9}", ""] {
            assert!(CurrencyCode::new(bad).is_err(), "{bad}");
        }
        assert!(CurrencyCode::new("A".repeat(MAX_CURRENCY_LEN + 1)).is_err());
        for good in ["main", "promo", "paid_2", "a-b"] {
            assert!(AccountLabel::new(good).is_ok(), "{good}");
        }
        for bad in ["Main", "a b", "a.b", ""] {
            assert!(AccountLabel::new(bad).is_err(), "{bad}");
        }
        assert_eq!(
            AccountLabel::new("x".repeat(MAX_LABEL_LEN + 1)),
            Err(Error::InvalidId {
                kind: IdKind::Label,
                problem: IdProblem::TooLong
            })
        );
    }

    #[test]
    fn meters_and_dimensions_have_the_names_alphabet() {
        for good in ["tokens.input", "compute.seconds", "model", "a_b-c.1"] {
            assert!(MeterId::new(good).is_ok(), "{good}");
            assert!(DimensionKey::new(good).is_ok(), "{good}");
        }
        for bad in ["Tokens", "a b", "a/b", "caf\u{e9}", ""] {
            assert!(MeterId::new(bad).is_err(), "{bad}");
            assert!(DimensionKey::new(bad).is_err(), "{bad}");
        }
        assert_eq!(
            MeterId::new("m".repeat(MAX_NAME_LEN + 1)),
            Err(Error::InvalidId {
                kind: IdKind::Meter,
                problem: IdProblem::TooLong
            })
        );
        assert!(DimensionValue::new("Example-Large/2").is_ok());
        assert!(DimensionValue::new("a b").is_err());
        assert!(DimensionValue::new("v".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn parses_and_converts() {
        let parsed: AccountId = "acct-1".parse().expect("valid id");
        let converted = AccountId::try_from(String::from("acct-1")).expect("valid id");
        assert_eq!(parsed, converted);
        assert_eq!(parsed.into_string(), "acct-1");
    }
}
