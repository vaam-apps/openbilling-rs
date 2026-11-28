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
    /// A raw event with a negative quantity.
    NegativeQuantity,
    /// A raw event whose meter has no price in its currency in the version in force at its occurrence time, or
    /// that occurred before any version (ADR 0004). Never priced at a guess.
    UnpricedMeter,
    /// A raw event whose meter is priced, but not for its dimensions: a key the meter is not priced by, a key it is
    /// priced by left out, or a value with no price (ADR 0004).
    UnpricedDimension,
    /// A price list that does not parse as YAML, or not as a price list: a syntax error, a missing or unknown key, a
    /// value of the wrong type. The one rule Rust decides: nothing reached the database.
    MalformedPriceList,
    /// A price list with a value outside its form: a name, a time, a price below zero, a `per` below one, no meter or
    /// no price, too many of either.
    InvalidPriceList,
    /// A price list that declares a meter, or a meter's dimension, twice, or prices one meter, currency and
    /// dimensions twice.
    PriceListDuplicate,
    /// A price list with a price for a meter it does not declare, or with dimensions that are not its meter's.
    PriceListMismatch,
    /// A version that would take effect before now, or at or before an event already rated (ADR 0001, point 19).
    BackDated,
    /// Another version already takes effect at the same instant.
    VersionConflict,
    /// A top-up recorded again with other terms than the first time (ADR 0003, amended 2026-11-28).
    TopUpConflict,
    /// An operator resolved a top-up that is not mismatched (ADR 0003, amended 2026-11-28).
    TopUpNotMismatched,
    /// A top-up already resolved the other way (ADR 0003, amended 2026-11-28).
    ResolutionConflict,
    /// A mismatched top-up no top-up was recorded for can only be dismissed, never credited (ADR 0003, amended
    /// 2026-11-28).
    NoRecordedTerms,
}

impl Rule {
    /// Every rule, in declaration order.
    pub const ALL: [Self; 30] = [
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
        Self::NegativeQuantity,
        Self::UnpricedMeter,
        Self::UnpricedDimension,
        Self::MalformedPriceList,
        Self::InvalidPriceList,
        Self::PriceListDuplicate,
        Self::PriceListMismatch,
        Self::BackDated,
        Self::VersionConflict,
        Self::TopUpConflict,
        Self::TopUpNotMismatched,
        Self::ResolutionConflict,
        Self::NoRecordedTerms,
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
            Self::NegativeQuantity => "negative_quantity",
            Self::UnpricedMeter => "unpriced_meter",
            Self::UnpricedDimension => "unpriced_dimension",
            Self::MalformedPriceList => "malformed_price_list",
            Self::InvalidPriceList => "invalid_price_list",
            Self::PriceListDuplicate => "price_list_duplicate",
            Self::PriceListMismatch => "price_list_mismatch",
            Self::BackDated => "back_dated",
            Self::VersionConflict => "version_conflict",
            Self::TopUpConflict => "top_up_conflict",
            Self::TopUpNotMismatched => "top_up_not_mismatched",
            Self::ResolutionConflict => "resolution_conflict",
            Self::NoRecordedTerms => "no_recorded_terms",
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
            Self::NegativeQuantity => "a negative quantity",
            Self::UnpricedMeter => "the meter has no price in the currency at that time",
            Self::UnpricedDimension => "the meter has no price for these dimensions",
            Self::MalformedPriceList => "it is not a price list",
            Self::InvalidPriceList => "a value outside its form",
            Self::PriceListDuplicate => "a meter, a dimension or a price given twice",
            Self::PriceListMismatch => {
                "a price for a meter not declared, or for dimensions that are not its meter's"
            }
            Self::BackDated => {
                "the version would take effect before now or before an event already rated"
            }
            Self::VersionConflict => "another version takes effect at the same time",
            Self::TopUpConflict => "the payment was already recorded as another top-up",
            Self::TopUpNotMismatched => "the top-up is not mismatched",
            Self::ResolutionConflict => "the top-up was resolved the other way",
            Self::NoRecordedTerms => {
                "no top-up was recorded for the payment: a mismatch like it can only be dismissed"
            }
        }
    }
}

impl Classify for Rule {
    fn class(&self) -> ErrorClass {
        match self {
            Self::PayloadConflict
            | Self::AccountConflict
            | Self::CreditConflict
            | Self::ReversalCap
            | Self::VersionConflict
            | Self::TopUpConflict
            | Self::TopUpNotMismatched
            | Self::ResolutionConflict => ErrorClass::Conflict,
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
            | Self::InvalidAmount
            | Self::NegativeQuantity
            | Self::UnpricedMeter
            | Self::UnpricedDimension
            | Self::MalformedPriceList
            | Self::InvalidPriceList
            | Self::PriceListDuplicate
            | Self::PriceListMismatch
            | Self::NoRecordedTerms
            | Self::BackDated => ErrorClass::Invalid,
        }
    }
}

/// Where in a price list a refusal points: a line and a column (1-based), and, when the database checked it, the key
/// of the value, for example `prices[2].currency`.
///
/// The key is built from the list's structure (field names and positions), never from its content, so a position
/// holds no value of the list (agent guide, rule 9).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ListPosition {
    line: u32,
    column: u32,
    key: Option<String>,
}

impl ListPosition {
    /// The longest key, in bytes.
    pub const MAX_KEY_LEN: usize = 128;

    /// A position at `line` and `column`, with no key: where the parser stopped.
    #[must_use]
    pub const fn at(line: u32, column: u32) -> Self {
        Self {
            line,
            column,
            key: None,
        }
    }

    /// A position at `line` and `column`, with `key`. `None` when the key is empty, longer than
    /// [`ListPosition::MAX_KEY_LEN`], or holds a byte other than `a`–`z`, `0`–`9`, `_`, `.`, `[` and `]`.
    #[must_use]
    pub fn with_key(line: u32, column: u32, key: &str) -> Option<Self> {
        let structural = !key.is_empty()
            && key.len() <= Self::MAX_KEY_LEN
            && key.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'.' | b'[' | b']')
            });
        structural.then(|| Self {
            line,
            column,
            key: Some(key.to_owned()),
        })
    }

    /// The line, from 1; 0 when unknown.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// The column, from 1; 0 when unknown.
    #[must_use]
    pub const fn column(&self) -> u32 {
        self.column
    }

    /// The key, when the database checked the value.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

impl fmt::Display for ListPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)?;
        if let Some(key) = &self.key {
            write!(f, " ({key})")?;
        }
        Ok(())
    }
}

/// A request refused by a rule. Nothing was written.
///
/// A refusal of a batch of events names the events that broke the rule, and only those (the card: every refusal
/// names the offending event). A refusal of a price list names its position in the list. Every other refusal names
/// nothing: an error never holds the value it refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    rule: Rule,
    events: Vec<EventId>,
    position: Option<ListPosition>,
}

impl Refusal {
    /// A refusal that names no event.
    #[must_use]
    pub const fn new(rule: Rule) -> Self {
        Self {
            rule,
            events: Vec::new(),
            position: None,
        }
    }

    /// A refusal of the events `events`.
    #[must_use]
    pub fn of_events(rule: Rule, events: Vec<EventId>) -> Self {
        Self {
            rule,
            events,
            position: None,
        }
    }

    /// A refusal of a price list at `position`.
    #[must_use]
    pub const fn at(rule: Rule, position: ListPosition) -> Self {
        Self {
            rule,
            events: Vec::new(),
            position: Some(position),
        }
    }

    /// Where in a price list, for a refusal of one.
    #[must_use]
    pub const fn position(&self) -> Option<&ListPosition> {
        self.position.as_ref()
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
        if let Some(position) = &self.position {
            return write!(
                f,
                "the price list is refused at {position}: {}",
                self.rule.describe()
            );
        }
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
    fn a_refusal_of_a_price_list_names_its_line_and_key() {
        let at = ListPosition::with_key(12, 5, "prices[2].currency").expect("a key");
        let refusal = Refusal::at(Rule::UnknownCurrency, at.clone());
        assert_eq!(
            refusal.to_string(),
            "the price list is refused at line 12, column 5 (prices[2].currency): a currency the operator has not named"
        );
        assert_eq!(refusal.position(), Some(&at));
        let parse = ListPosition::at(3, 1);
        assert_eq!(parse.to_string(), "line 3, column 1");
        assert_eq!(parse.key(), None);
        for bad in ["", "prices[2].Currency", "a b", "x\"y"] {
            assert_eq!(ListPosition::with_key(1, 1, bad), None, "{bad}");
        }
        assert_eq!(ListPosition::with_key(1, 1, &"k".repeat(129)), None);
    }

    #[test]
    fn rule_names_are_unique() {
        let mut names: Vec<_> = Rule::ALL.iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Rule::ALL.len());
        assert_eq!(Rule::VersionConflict.class(), ErrorClass::Conflict);
        assert_eq!(Rule::UnpricedMeter.class(), ErrorClass::Invalid);
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
