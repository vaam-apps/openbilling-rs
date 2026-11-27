//! What a worker decided about an event (ADR 0001, points 14, 15 and 23).

use std::fmt;

/// The outcome of one usage event, written once by the drain that decided it.
///
/// An event refused at intake (`too_old`, `too_far_ahead`, invalid) gets no row, so it has no outcome: those are
/// answers to the producer, not outcomes. The outcome record (ADR 0001, point 14) is `ob_event_outcome`; its ledger
/// transaction names the event. A raw event's outcome also names the price book version that rated it (ADR 0004).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventOutcome {
    /// Rated, and its ledger entries written.
    Applied,
    /// Not billed, for the reason given. A refusal is final for its event.
    Refused(RefusalReason),
}

impl EventOutcome {
    /// The outcome's stable name, the `state` an event row ends in: `applied` or `refused`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Refused(_) => "refused",
        }
    }

    /// The reason, for a refusal.
    #[must_use]
    pub const fn refusal_reason(self) -> Option<RefusalReason> {
        match self {
            Self::Applied => None,
            Self::Refused(reason) => Some(reason),
        }
    }
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a worker refused an event. Closed on purpose: a new reason must fail to compile wherever reasons are matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusalReason {
    /// No price in the price book version in effect at the event's occurrence time. Reserved: rating happens at
    /// intake, which refuses an unpriced event (`unpriced_meter`, `unpriced_dimension`), so no drain decides it
    /// (ADR 0004).
    NoPrice,
    /// The event names an account the service does not know.
    UnknownAccount,
    /// An outcome already exists for this event id, from an event on another occurrence day (ADR 0001, point 15).
    IdReused,
    /// The debit would take the owner's debt past the largest amount. Not the debt limit: since the owner's decision
    /// of 2026-11-27 ("Always book as debt") usage is booked as debt whatever the limit, which refuses only new calls
    /// through `allowed` (ADR 0001, amended, E1).
    Overflow,
}

impl RefusalReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::NoPrice,
        Self::UnknownAccount,
        Self::IdReused,
        Self::Overflow,
    ];

    /// The reason's stable name, as ADR 0001 writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoPrice => "no_price",
            Self::UnknownAccount => "unknown_account",
            Self::IdReused => "id_reused",
            Self::Overflow => "overflow",
        }
    }

    /// The reason of a stored name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == name)
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_those_of_adr_0001() {
        let names: Vec<_> = RefusalReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            ["no_price", "unknown_account", "id_reused", "overflow"]
        );
        assert_eq!(
            RefusalReason::parse("limit"),
            None,
            "no drain refuses for the limit"
        );
        for reason in RefusalReason::ALL {
            assert_eq!(RefusalReason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(RefusalReason::parse("other"), None);
    }

    #[test]
    fn outcome_names_are_the_final_states() {
        assert_eq!(EventOutcome::Applied.to_string(), "applied");
        for reason in RefusalReason::ALL {
            let outcome = EventOutcome::Refused(reason);
            assert_eq!(outcome.to_string(), "refused");
            assert_eq!(outcome.refusal_reason(), Some(reason));
        }
        assert_eq!(EventOutcome::Applied.refusal_reason(), None);
    }
}
