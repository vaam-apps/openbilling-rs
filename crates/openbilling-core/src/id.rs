//! Caller-given ids.
//!
//! An id is chosen by the caller, so the service must count it once whatever the caller sends (ADR 0001, point 1).
//! Both kinds follow one rule: 1 to [`MAX_ID_LEN`] bytes of printable ASCII, `!` (0x21) to `~` (0x7E). No
//! whitespace, no control character and no non-ASCII text, so two ids that look the same are the same bytes, and an
//! id can be written to a log without escaping.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, IdKind, IdProblem};

/// The longest id, in bytes.
pub const MAX_ID_LEN: usize = 128;

fn check(kind: IdKind, value: &str) -> Result<(), Error> {
    let problem = if value.is_empty() {
        Some(IdProblem::Empty)
    } else if value.len() > MAX_ID_LEN {
        Some(IdProblem::TooLong)
    } else if !value.bytes().all(|b| b.is_ascii_graphic()) {
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
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Checks `value` against the id rules and wraps it.
            ///
            /// # Errors
            ///
            /// [`Error::InvalidId`] when `value` is empty, longer than [`MAX_ID_LEN`] bytes, or holds a byte outside
            /// printable ASCII.
            pub fn new(value: impl Into<String>) -> Result<Self, Error> {
                let value = value.into();
                check($kind, &value)?;
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
    IdKind::Event
);

caller_id!(
    /// The id of an account that events are rated for and the ledger debits and credits.
    AccountId,
    IdKind::Account
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
    fn parses_and_converts() {
        let parsed: AccountId = "acct-1".parse().expect("valid id");
        let converted = AccountId::try_from(String::from("acct-1")).expect("valid id");
        assert_eq!(parsed, converted);
        assert_eq!(parsed.into_string(), "acct-1");
    }
}
