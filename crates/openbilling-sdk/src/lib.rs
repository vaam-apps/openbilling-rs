//! A Rust client for the [openbilling-rs](https://github.com/vaam-apps/openbilling-rs) API ([ADR 0008]).
//!
//! The typed calls are **generated** by CrateStack from the API's schema (`schema/openbilling.cstack`, a copy of the
//! server's that a test and CI hold equal): every operation of the API has a call, and every argument and answer is a
//! generated type, in [`generated::cratestack_schema`]. What is written by hand is a thin facade, [`Client`]:
//!
//! * the base URL and a bearer token ([`Client::set_token`]), sent as `Authorization: Bearer …`;
//! * an [`IdempotencyKey`] on every write, which the API requires, so a write cannot be sent without one;
//! * [`Error`], with an [`ErrorClass`] to decide from, which never holds the token.
//!
//! Amounts are CrateStack `BigInt`s: micro-units of the account's currency, decimal strings on the wire, `i64` by
//! `From<BigInt> for i64`, never floats.
//!
//! ```no_run
//! use openbilling_sdk::{Client, IdempotencyKey};
//! use openbilling_sdk::generated::cratestack_schema::{procedures, types};
//!
//! # async fn demo() -> Result<(), openbilling_sdk::Error> {
//! let client = Client::new("https://billing.example.com")?;
//! client.set_token("<a JWT your identity provider signed for the API>");
//!
//! // A read takes no key.
//! let balances = client
//!     .get_balance(&procedures::get_balance::Args {
//!         args: types::OwnerCurrency { owner: "tenant-1".into(), currency: "EUR".into() },
//!     })
//!     .await?;
//! println!("{} balance rows", balances.len());
//!
//! // A write takes the key: send the same one again to retry it.
//! let key = IdempotencyKey::new("open-acct-tenant-1")?;
//! # let _ = key;
//! # Ok(()) }
//! ```
//!
//! [ADR 0008]: https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0008-a-rust-sdk-on-crates-io.md

mod client;
mod error;

/// What CrateStack generates from `schema/openbilling.cstack`: the models, the types, the procedures' `Args` and
/// `Output` and the raw `Client`. [`Client::raw`] is the generated client, with the bearer token already attached.
// Generated code, not ours: its lints are CrateStack's to fix.
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    non_camel_case_types,
    non_snake_case
)]
pub mod generated {
    cratestack::include_client_schema!("schema/openbilling.cstack");
}

pub use client::{Client, IdempotencyKey};
pub use error::{Classify, Error, ErrorClass};
