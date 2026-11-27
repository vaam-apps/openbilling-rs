//! The parts of vpay's objects the adapter reads. Unknown fields are ignored, as vpay adds keys between versions.
//!
//! *Verified 2026-11-27*, vpay `8b56831`, `backends/crates/vpay-api/src/model.rs`: `PaymentIntentObject` (`id`
//! `pi_…`, `amount` in minor units, `currency` lower case, `status`, `metadata` of strings), `CheckoutSessionObject`
//! (`id` `cs_…`, `url`, null for an embedded session), `RefundObject` (`id` `re_…`, `amount`, `currency`,
//! `payment_intent`, `status`).

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct PaymentIntent {
    pub(crate) id: String,
    pub(crate) amount: i64,
    pub(crate) currency: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) metadata: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CheckoutSession {
    pub(crate) id: String,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Refund {
    pub(crate) id: String,
    pub(crate) amount: i64,
    pub(crate) currency: String,
    pub(crate) payment_intent: String,
    pub(crate) status: String,
}

/// vpay's error envelope, `{"error": {"type": …, "code": …}}`; only the type is read.
#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub(crate) error: ErrorBody,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorBody {
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
}
