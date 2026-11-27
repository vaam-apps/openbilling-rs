//! The usage event body, in one module: it changes when rating comes (the next slice), which adds a meter and a
//! quantity in place of the amount. In this slice an event is **pre-priced** (ADR 0001, amended 2026-11-27, B4).
//!
//! The schema's `UsageEventInput` carries the producer's event id (with the occurrence day, the once-only key),
//! the owner, the member, the currency, the amount in micro-units and the occurrence time. What the ledger
//! refuses (a negative amount, the window, a differing repeat) it refuses itself, naming the events.

use cratestack::CratestackError;
use openbilling_core::{
    Amount, CurrencyCode, EventId, Intake, IntakeReport, MemberId, OwnerId, UsageEvent,
};

use crate::errors::invalid;
use crate::generated::cratestack_schema::{EventAnswer, IntakeResult, UsageBatch};
use crate::time::timestamp;

/// The most events one call may carry: the ledger takes a batch of at most this many in one transaction.
pub const MAX_EVENTS: usize = 1000;

/// The batch, read into the ledger's events.
///
/// # Errors
///
/// `422` naming the field the ledger's types refuse, or for more than [`MAX_EVENTS`] events.
pub fn events(batch: &UsageBatch) -> Result<Vec<UsageEvent>, CratestackError> {
    if batch.events.len() > MAX_EVENTS {
        return Err(CratestackError::Validation(
            "events: more than 1000 events".to_owned(),
        ));
    }
    batch
        .events
        .iter()
        .map(|event| {
            Ok(UsageEvent {
                id: EventId::new(event.eventId.as_str()).map_err(|e| invalid("eventId", &e))?,
                owner: OwnerId::new(event.owner.as_str()).map_err(|e| invalid("owner", &e))?,
                currency: CurrencyCode::new(event.currency.as_str())
                    .map_err(|e| invalid("currency", &e))?,
                member: MemberId::new(event.member.as_str()).map_err(|e| invalid("member", &e))?,
                amount: Amount::from_micros(event.amountMicros.get())
                    .map_err(|e| invalid("amountMicros", &e))?,
                occurred_at: timestamp(&event.occurredAt, "occurredAt")?,
            })
        })
        .collect()
}

/// What intake answered, event by event, in the order sent.
#[must_use]
pub fn answer(report: &IntakeReport) -> IntakeResult {
    IntakeResult {
        accepted: i64::try_from(report.accepted()).unwrap_or(i64::MAX),
        replayed: i64::try_from(report.replayed()).unwrap_or(i64::MAX),
        answers: report
            .answers
            .iter()
            .map(|(id, answer)| EventAnswer {
                eventId: id.as_str().to_owned(),
                answer: match answer {
                    Intake::Accepted => "accepted",
                    Intake::Replayed => "replayed",
                }
                .to_owned(),
            })
            .collect(),
    }
}
