//! What the ledger refuses, and the error type of the [`LedgerStore`](crate::LedgerStore) port.

use std::fmt;

use crate::error::{Classify, ErrorClass};
use crate::id::EventId;

/// How many event ids a refusal's message names; the rest are counted.
pub const NAMED_IN_MESSAGE: usize = 10;

/// The rule a request broke. Closed on purpose: a new rule must fail to compile wherever rules are matched.
///
/// The rules themselves are PostgreSQL's (agent guide, rule 12); this is how a refusal is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A batch with more than 1000 events: one intake call is one transaction.
    TooManyEvents,
    /// An event with a negative amount.
    NegativeAmount,
    /// The same event id twice in one batch, with copies that differ.
    DifferingDuplicate,
    /// An event id already stored for its day with another payload (ADR 0001, point 2).
    PayloadConflict,
    /// An event that occurred before the window (ADR 0001, point 8).
    TooOld,
    /// An event that occurred after now plus the skew allowance.
    TooFarAhead,
    /// An event, an account or a cap in a currency the operator has not named.
    UnknownCurrency,
    /// Amounts that add up past the largest [`Amount`](crate::Amount): an owner's events in one batch, or a debt.
    Overflow,
    /// An account opened with a label the operator has not configured.
    UnknownLabel,
    /// A request that names an account, or an owner and currency, the ledger does not know.
    UnknownAccount,
    /// A reversal of a payment the ledger does not know.
    UnknownPayment,
    /// An account id already opened for another owner, currency or label, or an owner, currency and label that
    /// already have another account.
    AccountConflict,
    /// A credit, a reversal or an allowance whose id was already booked with other terms.
    CreditConflict,
    /// A reversal that would take back more than its payment, counting the reversals before it.
    ReversalCap,
    /// A configuration that leaves out a currency or a label already configured.
    IncompleteConfig,
    /// A configuration with no currency or no label, one named twice, or a lot order that does not name every lot
    /// kind once.
    InvalidConfig,
    /// An amount with the wrong sign for its source or kind, or an expiry on a credit that takes credit away.
    InvalidAmount,
}

impl Rule {
    /// Every rule, in declaration order.
    pub const ALL: [Self; 17] = [
        Self::TooManyEvents,
        Self::NegativeAmount,
        Self::DifferingDuplicate,
        Self::PayloadConflict,
        Self::TooOld,
        Self::TooFarAhead,
        Self::UnknownCurrency,
        Self::Overflow,
        Self::UnknownLabel,
        Self::UnknownAccount,
        Self::UnknownPayment,
        Self::AccountConflict,
        Self::CreditConflict,
        Self::ReversalCap,
        Self::IncompleteConfig,
        Self::InvalidConfig,
        Self::InvalidAmount,
    ];

    /// The rule's stable name, for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooManyEvents => "too_many_events",
            Self::NegativeAmount => "negative_amount",
            Self::DifferingDuplicate => "differing_duplicate",
            Self::PayloadConflict => "payload_conflict",
            Self::TooOld => "too_old",
            Self::TooFarAhead => "too_far_ahead",
            Self::UnknownCurrency => "unknown_currency",
            Self::Overflow => "overflow",
            Self::UnknownLabel => "unknown_label",
            Self::UnknownAccount => "unknown_account",
            Self::UnknownPayment => "unknown_payment",
            Self::AccountConflict => "account_conflict",
            Self::CreditConflict => "credit_conflict",
            Self::ReversalCap => "reversal_cap",
            Self::IncompleteConfig => "incomplete_config",
            Self::InvalidConfig => "invalid_config",
            Self::InvalidAmount => "invalid_amount",
        }
    }

    /// What breaking this rule says, in fixed text.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::TooManyEvents => "the batch holds too many events",
            Self::NegativeAmount => "a negative amount",
            Self::DifferingDuplicate => "a repeat that differs from its first copy in the batch",
            Self::PayloadConflict => "an id already recorded for its day with another payload",
            Self::TooOld => "it occurred before the window",
            Self::TooFarAhead => "it occurred too far in the future",
            Self::UnknownCurrency => "a currency the operator has not named",
            Self::Overflow => "amounts that add up past the largest amount",
            Self::UnknownLabel => "an account label the operator has not configured",
            Self::UnknownAccount => "no such account",
            Self::UnknownPayment => "no such payment",
            Self::AccountConflict => {
                "the account id or the owner's label is taken with other terms"
            }
            Self::CreditConflict => "the id was already booked with other terms",
            Self::ReversalCap => "it would reverse more than the payment",
            Self::IncompleteConfig => {
                "the configuration leaves out a currency or label already configured"
            }
            Self::InvalidConfig => "the configuration is invalid",
            Self::InvalidAmount => "the amount or expiry does not fit its source or kind",
        }
    }
}

impl Classify for Rule {
    fn class(&self) -> ErrorClass {
        match self {
            Self::PayloadConflict
            | Self::AccountConflict
            | Self::CreditConflict
            | Self::ReversalCap => ErrorClass::Conflict,
            Self::TooManyEvents
            | Self::NegativeAmount
            | Self::DifferingDuplicate
            | Self::TooOld
            | Self::TooFarAhead
            | Self::UnknownCurrency
            | Self::Overflow
            | Self::UnknownLabel
            | Self::UnknownAccount
            | Self::UnknownPayment
            | Self::IncompleteConfig
            | Self::InvalidConfig
            | Self::InvalidAmount => ErrorClass::Invalid,
        }
    }
}

/// A request refused by a rule. Nothing was written.
///
/// A refusal of a batch of events names the events that broke the rule, and only those (the card: every refusal
/// names the offending event). Every other refusal names nothing: an error never holds the value it refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    rule: Rule,
    events: Vec<EventId>,
}

impl Refusal {
    /// A refusal that names no event.
    #[must_use]
    pub const fn new(rule: Rule) -> Self {
        Self {
            rule,
            events: Vec::new(),
        }
    }

    /// A refusal of the events `events`.
    #[must_use]
    pub fn of_events(rule: Rule, events: Vec<EventId>) -> Self {
        Self { rule, events }
    }

    /// The rule.
    #[must_use]
    pub const fn rule(&self) -> Rule {
        self.rule
    }

    /// The events that broke it, in batch order. Empty for a refusal that is not about events.
    #[must_use]
    pub fn events(&self) -> &[EventId] {
        &self.events
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.events.is_empty() {
            return write!(f, "refused: {}", self.rule.describe());
        }
        f.write_str("the batch is refused: event ")?;
        for (n, event) in self.events.iter().take(NAMED_IN_MESSAGE).enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            f.write_str(event.as_str())?;
        }
        if self.events.len() > NAMED_IN_MESSAGE {
            write!(f, " and {} more", self.events.len() - NAMED_IN_MESSAGE)?;
        }
        write!(f, ": {}", self.rule.describe())
    }
}

impl Classify for Refusal {
    fn class(&self) -> ErrorClass {
        self.rule.class()
    }
}

/// The error of every [`LedgerStore`](crate::LedgerStore) call: a refusal, or a fault of the store.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// A rule refused the request; nothing was written.
    #[error("{0}")]
    Refused(Refusal),
    /// The store failed. The message is fixed text; the cause is the [`source`](std::error::Error::source).
    #[error("the ledger store failed ({class})")]
    Store {
        /// What a caller should do about it.
        class: ErrorClass,
        /// The adapter's own error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl LedgerError {
    /// The refusal, when a rule refused the request.
    #[must_use]
    pub const fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Store { .. } => None,
        }
    }

    /// The rule, when a rule refused the request.
    #[must_use]
    pub const fn rule(&self) -> Option<Rule> {
        match self {
            Self::Refused(refusal) => Some(refusal.rule),
            Self::Store { .. } => None,
        }
    }
}

impl From<Refusal> for LedgerError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<Rule> for LedgerError {
    fn from(rule: Rule) -> Self {
        Self::Refused(Refusal::new(rule))
    }
}

impl Classify for LedgerError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Refused(refusal) => refusal.class(),
            Self::Store { class, .. } => *class,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<EventId> {
        (0..n)
            .map(|i| EventId::new(format!("e{i}")).expect("an id"))
            .collect()
    }

    #[test]
    fn a_refusal_names_its_events_up_to_ten_and_counts_the_rest() {
        let one = Refusal::of_events(Rule::TooOld, ids(1));
        assert_eq!(
            one.to_string(),
            "the batch is refused: event e0: it occurred before the window"
        );
        let many = Refusal::of_events(Rule::NegativeAmount, ids(12));
        let text = many.to_string();
        assert!(text.contains("e9 and 2 more"), "{text}");
        assert!(!text.contains("e10"), "{text}");
    }

    #[test]
    fn conflicts_are_conflicts_and_the_rest_invalid() {
        assert_eq!(Rule::PayloadConflict.class(), ErrorClass::Conflict);
        assert_eq!(Rule::ReversalCap.class(), ErrorClass::Conflict);
        assert_eq!(Rule::TooOld.class(), ErrorClass::Invalid);
        let error = LedgerError::from(Rule::CreditConflict);
        assert_eq!(error.class(), ErrorClass::Conflict);
        assert_eq!(error.rule(), Some(Rule::CreditConflict));
        assert_eq!(
            error.to_string(),
            "refused: the id was already booked with other terms"
        );
        let store = LedgerError::Store {
            class: ErrorClass::Unavailable,
            source: "postgres://user:secret@db".into(),
        };
        assert_eq!(store.class(), ErrorClass::Unavailable);
        assert!(!store.to_string().contains("secret"));
        assert_eq!(store.rule(), None);
    }
}
