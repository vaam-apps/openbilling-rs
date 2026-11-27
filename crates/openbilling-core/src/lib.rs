//! Pure types and rules of openbilling-rs.
//!
//! This crate does no I/O and has no async code. It holds the vocabulary of
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md)
//! that every other crate shares:
//!
//! - [`EventId`] and [`AccountId`], the caller-given ids;
//! - [`Amount`], money as an integer number of minor units;
//! - [`EventOutcome`] and [`RefusalReason`], what a worker decided about an event;
//! - [`Error`], [`ErrorClass`] and [`Classify`], errors that tell a caller what to do.

mod amount;
mod error;
mod id;
mod outcome;

pub use amount::Amount;
pub use error::{Classify, Error, ErrorClass, IdKind, IdProblem};
pub use id::{AccountId, EventId, MAX_ID_LEN};
pub use outcome::{EventOutcome, RefusalReason};
