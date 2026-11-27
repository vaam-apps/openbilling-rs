//! The errors of the ports, as CrateStack answers them: decided from the class (agent guide, rule 9), never the
//! variant. `invalid` is `422` (`VALIDATION_ERROR`), `conflict` `409`, `unavailable` `503`, `internal` `500` with
//! CrateStack's canned message. The message is the refusal's fixed text, which names the rule and, for a batch,
//! the events it refused; it never holds a value the caller sent or a secret.

use cratestack::CratestackError;
use openbilling_core::{Classify, ErrorClass, LedgerError, PaymentError, SettleError};

/// The CrateStack error of a class, with `message`.
#[must_use]
pub fn of_class(class: ErrorClass, message: String) -> CratestackError {
    match class {
        ErrorClass::Invalid => CratestackError::Validation(message),
        ErrorClass::Conflict => CratestackError::Conflict(message),
        ErrorClass::Unavailable => CratestackError::Unavailable(message),
        ErrorClass::Internal => CratestackError::Internal(message),
    }
}

/// A ledger error: the rule's name and its fixed sentence.
#[must_use]
pub fn ledger(error: &LedgerError) -> CratestackError {
    let message = match error.rule() {
        Some(rule) => format!("{}: {error}", rule.as_str()),
        None => error.to_string(),
    };
    of_class(error.class(), message)
}

/// A payment provider error: the refusal's name and its fixed sentence.
#[must_use]
pub fn payment(error: &PaymentError) -> CratestackError {
    let message = match error.refusal() {
        Some(refusal) => format!("{}: {error}", refusal.as_str()),
        None => error.to_string(),
    };
    of_class(error.class(), message)
}

/// A settlement error.
#[must_use]
pub fn settle(error: &SettleError) -> CratestackError {
    match error {
        SettleError::Provider(error) => payment(error),
        SettleError::Ledger(error) => ledger(error),
    }
}

/// A request value the ledger's types refuse (an id outside its alphabet, an amount out of range).
#[must_use]
pub fn invalid(field: &str, error: &openbilling_core::Error) -> CratestackError {
    CratestackError::Validation(format!("{field}: {error}"))
}

#[cfg(test)]
mod tests {
    use openbilling_core::{EventId, Refusal, Rule};

    use super::*;

    #[test]
    fn classes_choose_the_answer_and_a_fault_says_nothing_of_its_cause() {
        let refused = LedgerError::Refused(Refusal::of_events(
            Rule::TooOld,
            vec![EventId::new("e1").expect("an id")],
        ));
        let CratestackError::Validation(message) = ledger(&refused) else {
            panic!("422")
        };
        assert_eq!(
            message,
            "too_old: the batch is refused: event e1: it occurred before the window"
        );
        assert!(matches!(
            ledger(&LedgerError::from(Rule::CreditConflict)),
            CratestackError::Conflict(_)
        ));
        let fault = ledger(&LedgerError::Store {
            class: ErrorClass::Unavailable,
            source: "postgres://user:secret@db".into(),
        });
        let CratestackError::Unavailable(message) = fault else {
            panic!("503")
        };
        assert!(!message.contains("secret"), "{message}");
    }
}
