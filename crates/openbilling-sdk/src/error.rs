//! [`Error`] and its [`ErrorClass`].

use std::fmt;

use cratestack::client_rust::ClientError;

/// What a caller does about an [`Error`]. The same four classes as the service's own (`openbilling_core::ErrorClass`,
/// which this crate does not depend on); closed on purpose, so a new class fails to compile wherever it is matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// The request is wrong, or the caller may not make it (`400`, `401`, `403`, `404`, `422`). Sending it again
    /// unchanged gives the same answer.
    Invalid,
    /// The request disagrees with what the service already recorded (`409`): the same id with another payload, a key
    /// still in flight. Sending it again unchanged gives the same answer, apart from an in-flight key.
    Conflict,
    /// The service cannot answer now (`503`, a refused or dropped connection, a timeout). The same request, with the
    /// same `Idempotency-Key`, may succeed later.
    Unavailable,
    /// A fault of the service (`500`) or an answer this client cannot read. Not the caller's to fix.
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

/// An error that knows its [`ErrorClass`].
pub trait Classify {
    /// The class of this error.
    fn class(&self) -> ErrorClass;
}

/// Why a call failed. Never holds the bearer token: the token lives in the client and is read only to build a
/// request's header.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The base URL is not a URL this client can use.
    #[error("invalid base URL: {0}")]
    BaseUrl(String),
    /// An `Idempotency-Key` that is empty, too long or not visible ASCII.
    #[error("invalid idempotency key: {0}")]
    IdempotencyKey(&'static str),
    /// The service answered with a failure status. `message` is the service's own text, which names a rule
    /// (`unknown_account: …`) and never a value it refused.
    #[error("the service answered {status}: {message}")]
    Remote {
        /// The HTTP status.
        status: u16,
        /// The service's message.
        message: String,
    },
    /// The request did not reach the service, or no answer came back.
    #[error("could not reach the service: {0}")]
    Transport(String),
    /// The answer could not be read as the schema says.
    #[error("unreadable answer: {0}")]
    Decode(String),
}

impl Error {
    /// The HTTP status of a [`Error::Remote`].
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Remote { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::BaseUrl(_) | Self::IdempotencyKey(_) => ErrorClass::Invalid,
            Self::Remote { status, .. } => match *status {
                409 => ErrorClass::Conflict,
                429 | 502..=504 => ErrorClass::Unavailable,
                400..=499 => ErrorClass::Invalid,
                _ => ErrorClass::Internal,
            },
            Self::Transport(_) => ErrorClass::Unavailable,
            Self::Decode(_) => ErrorClass::Internal,
        }
    }
}

impl From<ClientError> for Error {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Remote {
                status, message, ..
            } => Self::Remote {
                status: status.as_u16(),
                message,
            },
            ClientError::Transport(e) => Self::Transport(e.to_string()),
            ClientError::BadInput(m) => Self::Decode(m),
            other => Self::Decode(other.to_string()),
        }
    }
}
