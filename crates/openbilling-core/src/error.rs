//! Errors and their classes.

use std::fmt;

/// What a caller should do about an error. Callers decide from the class, never from the variant.
///
/// The enum is closed on purpose: a new class must fail to compile wherever classes are matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// The request is wrong. Retrying it unchanged gives the same answer.
    Invalid,
    /// The request disagrees with what is already recorded, for example the same id with another payload
    /// (ADR 0001, points 2 and 25). Retrying it unchanged gives the same answer.
    Conflict,
    /// The service cannot answer now, for example PostgreSQL is unreachable. The same request may succeed later.
    Unavailable,
    /// A fault of the service itself. Not the caller's to fix.
    Internal,
}

impl ErrorClass {
    /// The class's stable name, for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error that knows its [`ErrorClass`]. Every error type of the workspace implements it.
pub trait Classify {
    /// The class of this error.
    fn class(&self) -> ErrorClass;
}

/// Which kind of id an [`Error::InvalidId`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdKind {
    /// An [`EventId`](crate::EventId).
    Event,
    /// An [`AccountId`](crate::AccountId).
    Account,
    /// An [`OwnerId`](crate::OwnerId).
    Owner,
    /// A [`MemberId`](crate::MemberId).
    Member,
    /// A [`CreditId`](crate::CreditId).
    Credit,
    /// A [`CurrencyCode`](crate::CurrencyCode).
    Currency,
    /// An [`AccountLabel`](crate::AccountLabel).
    Label,
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Event => "event",
            Self::Account => "account",
            Self::Owner => "owner",
            Self::Member => "member",
            Self::Credit => "credit",
            Self::Currency => "currency",
            Self::Label => "label",
        })
    }
}

/// Why an id was refused. The value itself is never part of the error, so an error can be logged as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdProblem {
    /// The id is empty.
    Empty,
    /// The id is longer than [`MAX_ID_LEN`](crate::MAX_ID_LEN) bytes.
    TooLong,
    /// The id holds a byte its kind does not allow: for an id, one outside printable ASCII (`!` to `~`); for a
    /// currency or a label, one outside its narrower alphabet.
    ForbiddenCharacter,
}

impl fmt::Display for IdProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "it is empty",
            Self::TooLong => "it is too long",
            Self::ForbiddenCharacter => "it holds a character its kind does not allow",
        })
    }
}

/// The errors of `openbilling-core`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// An id does not meet the rules of [`EventId`](crate::EventId) and [`AccountId`](crate::AccountId).
    #[error("{kind} id is invalid: {problem}")]
    InvalidId {
        /// Which id.
        kind: IdKind,
        /// What is wrong with it.
        problem: IdProblem,
    },
    /// A number of micro-units outside the range of [`Amount`](crate::Amount).
    #[error("amount is out of range")]
    AmountOutOfRange,
    /// A number of microseconds outside the range of [`Timestamp`](crate::Timestamp).
    #[error("timestamp is out of range")]
    TimestampOutOfRange,
    /// A window of zero days.
    #[error("the intake window is invalid")]
    InvalidWindow,
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidId { .. }
            | Self::AmountOutOfRange
            | Self::TimestampOutOfRange
            | Self::InvalidWindow => ErrorClass::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_are_stable() {
        let names: Vec<_> = [
            ErrorClass::Invalid,
            ErrorClass::Conflict,
            ErrorClass::Unavailable,
            ErrorClass::Internal,
        ]
        .iter()
        .map(|c| c.to_string())
        .collect();
        assert_eq!(names, ["invalid", "conflict", "unavailable", "internal"]);
    }

    #[test]
    fn core_errors_are_the_callers_to_fix() {
        let invalid_id = Error::InvalidId {
            kind: IdKind::Event,
            problem: IdProblem::Empty,
        };
        assert_eq!(invalid_id.class(), ErrorClass::Invalid);
        assert_eq!(Error::AmountOutOfRange.class(), ErrorClass::Invalid);
    }

    #[test]
    fn an_invalid_id_error_names_the_kind_and_the_problem() {
        let error = Error::InvalidId {
            kind: IdKind::Account,
            problem: IdProblem::TooLong,
        };
        assert_eq!(error.to_string(), "account id is invalid: it is too long");
    }
}
