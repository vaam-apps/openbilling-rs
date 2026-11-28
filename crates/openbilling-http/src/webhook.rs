//! The payment provider's webhook: a plain axum route beside the CrateStack router, because it needs the raw
//! body and the signature header exactly as sent.
//!
//! It verifies the signature over the raw body (the provider's own scheme, in the adapter), reads the object it
//! names back from the provider, and books what the provider says succeeded through the ledger's credit or
//! reversal function (ADR 0003). A webhook is a hint: nothing in its body is believed but the id.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cratestack::CratestackError;
use cratestack::axum::Json;
use cratestack::axum::body::Bytes;
use cratestack::axum::extract::State;
use cratestack::axum::http::{HeaderMap, StatusCode};
use cratestack::axum::response::{IntoResponse, Response};
use openbilling_core::{
    LedgerStore, PaymentError, PaymentProvider, PaymentRefusal, SettleError, Timestamp,
    TopUpLedger, WebhookHeaders, settle_hint,
};

use crate::generated::cratestack_schema::Settlement;
use crate::procedures::{Services, settlement as settlement_of};

/// The largest webhook body read, in bytes.
pub const WEBHOOK_BODY_LIMIT: usize = 64 * 1024;

/// An error as the rest of the API writes it: CrateStack's envelope, with its status.
pub(crate) fn answer(error: CratestackError) -> Response {
    let status = error.status_code();
    let challenges: Vec<String> = error.auth_challenges().to_vec();
    let mut response = (status, Json(error.into_response())).into_response();
    for challenge in challenges {
        if let Ok(value) = challenge.parse() {
            response
                .headers_mut()
                .append(cratestack::axum::http::header::WWW_AUTHENTICATE, value);
        }
    }
    response
}

fn now() -> Result<Timestamp, CratestackError> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok())
        .ok_or_else(|| CratestackError::Internal("the clock is before 1970".to_owned()))?;
    Timestamp::from_unix_micros(micros)
        .map_err(|_| CratestackError::Internal("the clock is out of range".to_owned()))
}

fn refused(error: &PaymentError) -> CratestackError {
    match error.refusal() {
        Some(PaymentRefusal::WebhookSignature | PaymentRefusal::WebhookTimestamp) => {
            CratestackError::Unauthorized(format!("{error}"))
        }
        Some(PaymentRefusal::WebhookBody) => CratestackError::BadRequest(format!("{error}")),
        _ => crate::errors::payment(error),
    }
}

/// `POST /webhooks/payments`.
pub(crate) async fn receive<
    L: LedgerStore + TopUpLedger + 'static,
    P: PaymentProvider + 'static,
>(
    State(services): State<Arc<Services<L, P>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(provider) = services.provider.as_ref() else {
        return answer(CratestackError::Unavailable(
            "no payment provider is configured".to_owned(),
        ));
    };
    let at = match now() {
        Ok(at) => at,
        Err(error) => return answer(error),
    };
    let headers = WebhookHeaders::new(headers.iter().filter_map(|(name, value)| {
        value
            .to_str()
            .ok()
            .map(|value| (name.as_str().to_owned(), value.to_owned()))
    }));
    let hint = match provider.verify_webhook(&headers, &body, at) {
        Ok(hint) => hint,
        Err(error) => return answer(refused(&error)),
    };
    match settle_hint(&services.ledger, provider, &hint).await {
        Ok(settled) => (StatusCode::OK, Json(settlement_of(&settled))).into_response(),
        // A payment that differs from its top-up is acknowledged, as a payment not settled is: the refusal is final
        // (the ledger marked it, nothing was credited), and an error status would only have the provider send the
        // same webhook again. The operator's `reconcilePayment` answers it as a conflict.
        Err(SettleError::ReadBackMismatch(_)) => (
            StatusCode::OK,
            Json(Settlement {
                outcome: "mismatch".to_owned(),
                accountId: None,
                transaction: None,
                replayed: false,
            }),
        )
            .into_response(),
        Err(error) => answer(crate::errors::settle(&error)),
    }
}
