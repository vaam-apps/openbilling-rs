//! The usage event bodies, in one module: a **pre-priced** event (`recordUsage`, ADR 0001, amended 2026-11-27, B4)
//! and a **raw** one (`recordRawUsage`, ADR 0004), which carries a meter, a quantity and dimensions in place of the
//! amount and is rated at intake.
//!
//! The schema's `UsageEventInput` carries the producer's event id (with the occurrence day, the once-only key),
//! the owner, the member, the currency, the amount in micro-units and the occurrence time; `RawUsageEventInput`
//! the same with a meter, a quantity and dimensions. What the ledger refuses (a negative amount, the window, a
//! differing repeat, an unpriced meter) it refuses itself, naming the events.

use cratestack::CratestackError;
use openbilling_core::{
    Amount, CurrencyCode, DimensionKey, DimensionValue, Dimensions, EventId, Intake, IntakeReport,
    MemberId, MeterId, OwnerId, Quantity, RawEvent, UsageEvent,
};

use crate::errors::invalid;
use crate::generated::cratestack_schema::{EventAnswer, IntakeResult, RawUsageBatch, UsageBatch};
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

/// The raw batch, read into the ledger's raw events.
///
/// # Errors
///
/// `422` naming the field the ledger's types refuse, a dimension key given twice, or more than [`MAX_EVENTS`]
/// events.
pub fn raw_events(batch: &RawUsageBatch) -> Result<Vec<RawEvent>, CratestackError> {
    if batch.events.len() > MAX_EVENTS {
        return Err(CratestackError::Validation(
            "events: more than 1000 events".to_owned(),
        ));
    }
    batch
        .events
        .iter()
        .map(|event| {
            let mut dimensions = Dimensions::none();
            for dimension in &event.dimensions {
                let key = DimensionKey::new(dimension.key.as_str())
                    .map_err(|e| invalid("dimensions.key", &e))?;
                let value = DimensionValue::new(dimension.value.as_str())
                    .map_err(|e| invalid("dimensions.value", &e))?;
                let before = dimensions.len();
                dimensions = dimensions.with(key, value);
                if dimensions.len() == before {
                    return Err(CratestackError::Validation(
                        "dimensions: a key is given twice".to_owned(),
                    ));
                }
            }
            Ok(RawEvent {
                id: EventId::new(event.eventId.as_str()).map_err(|e| invalid("eventId", &e))?,
                owner: OwnerId::new(event.owner.as_str()).map_err(|e| invalid("owner", &e))?,
                currency: CurrencyCode::new(event.currency.as_str())
                    .map_err(|e| invalid("currency", &e))?,
                member: MemberId::new(event.member.as_str()).map_err(|e| invalid("member", &e))?,
                meter: MeterId::new(event.meter.as_str()).map_err(|e| invalid("meter", &e))?,
                quantity: Quantity::new(event.quantity.get())
                    .map_err(|e| invalid("quantity", &e))?,
                dimensions,
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
