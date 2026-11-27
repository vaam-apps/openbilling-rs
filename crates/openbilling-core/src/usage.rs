//! Usage events and what intake answers for them (ADR 0001, points 1 to 3).
//!
//! A [`UsageEvent`] is **pre-priced**: it carries its amount, for a producer that rates elsewhere. A raw event, a
//! quantity of a meter rated by the price book at intake, is a [`RawEvent`](crate::RawEvent) (ADR 0004); both go
//! through the same intake and drain.
//!
//! The rules a batch meets (at most 1000 events, no negative amount, a repeated id only as an identical copy, sums
//! that fit, a configured currency, the window, no conflict with a stored payload) and the payload hash are
//! PostgreSQL's (`ob_intake`, agent guide, rule 12).

use crate::amount::Amount;
use crate::id::{CurrencyCode, EventId, MemberId, OwnerId};
use crate::time::Timestamp;

/// One priced usage event. The owner and the currency say whose accounts it debits (ADR 0002); the member, whose
/// cap it counts against.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsageEvent {
    /// The caller-given id. With the occurrence day, the once-only key.
    pub id: EventId,
    /// The owner whose accounts pay for it.
    pub owner: OwnerId,
    /// The currency of `amount`, and of the accounts it draws.
    pub currency: CurrencyCode,
    /// The member who caused it.
    pub member: MemberId,
    /// What it costs, in micro-units. The store refuses a negative one.
    pub amount: Amount,
    /// When it occurred.
    pub occurred_at: Timestamp,
}

/// What intake answered for one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intake {
    /// Written now.
    Accepted,
    /// Already written with the same payload, by an earlier call or earlier in this batch. Nothing changed.
    Replayed,
}

/// What intake answered for a batch, event by event, in input order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntakeReport {
    /// The answers, in input order.
    pub answers: Vec<(EventId, Intake)>,
}

impl IntakeReport {
    /// How many events were written now.
    #[must_use]
    pub fn accepted(&self) -> usize {
        self.answers
            .iter()
            .filter(|(_, answer)| *answer == Intake::Accepted)
            .count()
    }

    /// How many were replays.
    #[must_use]
    pub fn replayed(&self) -> usize {
        self.answers.len() - self.accepted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_counts_its_answers() {
        let id = |v: &str| EventId::new(v).expect("id");
        let report = IntakeReport {
            answers: vec![
                (id("a"), Intake::Accepted),
                (id("b"), Intake::Replayed),
                (id("a"), Intake::Replayed),
            ],
        };
        assert_eq!((report.accepted(), report.replayed()), (1, 2));
        assert_eq!(IntakeReport::default().accepted(), 0);
    }
}
