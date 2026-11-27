//! Pure types and rules of openbilling-rs.
//!
//! This crate does no I/O and has no async runtime. It holds the vocabulary of
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md) and
//! [ADR 0002](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0002-an-owner-holds-several-accounts.md)
//! that every other crate shares:
//!
//! - ids: [`EventId`], [`AccountId`], [`OwnerId`], [`MemberId`], [`CreditId`], and the operator's names
//!   [`CurrencyCode`] and [`AccountLabel`];
//! - [`Amount`], money as an integer number of micro-units, and [`Timestamp`], [`UtcDay`] and [`UtcMonth`];
//! - the configuration: [`LedgerConfig`], [`LotOrder`], [`Window`], [`CreditSource`], [`LotKind`];
//! - [`UsageEvent`], [`Credit`], [`Reversal`] and what the ledger answers for them;
//! - [`EventOutcome`] and [`RefusalReason`], what a drain decided about an event;
//! - the storage port, [`LedgerStore`], with [`LedgerError`] and its [`Refusal`]s, and [`AllowedPolicy`] and
//!   [`RefuseWhen`], the deployer's choice of how [`LedgerStore::allowed`] compares;
//! - the payment provider port, [`PaymentProvider`] (ADR 0003), with [`PaymentError`], and [`settle_payment`] and
//!   [`settle_refund`], which book what a provider, read back, says succeeded;
//! - the price book (ADR 0004): [`RawEvent`], [`Quantity`], [`Dimensions`], [`MeterId`], [`Rated`], [`Loaded`],
//!   and its port, [`PriceBook`];
//! - [`Error`], [`ErrorClass`] and [`Classify`], errors that tell a caller what to do.
//!
//! The ledger's rules are not here. They are PostgreSQL's, written once in `openbilling-pg`'s migrations (agent
//! guide, rule 12); this crate names what goes in and out of them.

mod amount;
mod config;
mod error;
mod id;
mod outcome;
mod payment;
mod price;
mod refusal;
mod settle;
mod store;
mod time;
mod usage;

pub use amount::Amount;
pub use config::{CreditSource, LedgerConfig, LotKind, LotOrder, ReversalKind, Window, WithinKind};
pub use error::{Classify, Error, ErrorClass, IdKind, IdProblem};
pub use id::{
    AccountId, AccountLabel, CreditId, CurrencyCode, DimensionKey, DimensionValue, EventId,
    MAX_CURRENCY_LEN, MAX_ID_LEN, MAX_LABEL_LEN, MAX_NAME_LEN, MAX_OWNER_LEN, MemberId, MeterId,
    OwnerId,
};
pub use outcome::{EventOutcome, RefusalReason};
pub use payment::{
    Checkout, CreatedPayment, IdempotencyKey, MAX_IDEMPOTENCY_KEY_LEN, PaymentError,
    PaymentProvider, PaymentRefusal, PaymentRequest, PaymentStatus, PaymentTarget, PaymentView,
    RefundStatus, RefundView, ReturnUrls, WebhookHeaders, WebhookHint,
};
pub use price::{
    Dimensions, Loaded, PriceBook, PriceVersionId, PriceVersionView, Quantity, Rated, RawEvent,
};
pub use refusal::{LedgerError, ListPosition, NAMED_IN_MESSAGE, Refusal, Rule};
pub use settle::{SettleError, Settled, settle_hint, settle_payment, settle_refund};
pub use store::{
    AccountBalance, Allowed, AllowedPolicy, Balance, Credit, CreditTarget, Credited, Decided,
    DrainReport, LedgerStore, LotView, NewAccount, NotAllowed, Opened, OutcomeView, RefuseWhen,
    Reversal,
};
pub use time::{Timestamp, UtcDay, UtcMonth};
pub use usage::{Intake, IntakeReport, UsageEvent};
