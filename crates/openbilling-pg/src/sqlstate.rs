//! What the ledger's SQL raises, mapped to a refusal or an error class: the one place it is mapped.
//!
//! The migrations raise SQLSTATEs of class `OB` (five upper-case letters and digits, outside the classes the SQL
//! standard reserves, which PL/pgSQL allows: *verified 2026-11-27*, PostgreSQL 19 documentation, 41.9 *Errors and
//! Messages*, and Appendix A). An `OB1xx` refusal names its events in the error's DETAIL, separated by one space; an
//! id holds no space, so the split is exact.

use openbilling_core::{ErrorClass, EventId, LedgerError, Refusal, Rule};

/// What a SQLSTATE of the ledger means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meaning {
    /// A rule refused the request. `names_events`: the DETAIL lists the events that broke it.
    Refused {
        /// The rule.
        rule: Rule,
        /// Whether DETAIL names events.
        names_events: bool,
    },
    /// A fault of the store: an invariant the SQL holds was about to break.
    Fault(ErrorClass),
}

const fn refused(rule: Rule) -> Meaning {
    Meaning::Refused {
        rule,
        names_events: false,
    }
}

const fn refused_naming(rule: Rule) -> Meaning {
    Meaning::Refused {
        rule,
        names_events: true,
    }
}

/// Every SQLSTATE the migrations raise, and what it means.
pub const LEDGER_SQLSTATES: [(&str, Meaning); 21] = [
    ("OB001", refused(Rule::CreditConflict)),
    ("OB002", refused(Rule::UnknownAccount)),
    ("OB003", refused(Rule::UnknownCurrency)),
    ("OB004", refused(Rule::ReversalCap)),
    ("OB005", refused(Rule::IncompleteConfig)),
    ("OB006", refused(Rule::UnknownLabel)),
    ("OB007", refused(Rule::UnknownPayment)),
    ("OB008", refused(Rule::AccountConflict)),
    ("OB009", refused(Rule::InvalidAmount)),
    ("OB010", refused(Rule::InvalidConfig)),
    ("OB101", refused(Rule::TooManyEvents)),
    ("OB102", refused_naming(Rule::NegativeAmount)),
    ("OB103", refused_naming(Rule::DifferingDuplicate)),
    ("OB104", refused_naming(Rule::PayloadConflict)),
    ("OB105", refused_naming(Rule::TooOld)),
    ("OB106", refused_naming(Rule::TooFarAhead)),
    ("OB107", refused_naming(Rule::UnknownCurrency)),
    ("OB108", refused_naming(Rule::Overflow)),
    ("OB900", Meaning::Fault(ErrorClass::Internal)),
    ("OB901", Meaning::Fault(ErrorClass::Internal)),
    ("OB902", Meaning::Fault(ErrorClass::Internal)),
];

/// What `code` means, among the ledger's own SQLSTATEs and the standard ones the ledger relies on:
///
/// - `22003` (`numeric_value_out_of_range`): a debt past the largest amount, refused as `overflow`;
/// - `40001` and `40P01` (serialization failure, deadlock): the same call may succeed, `unavailable`;
/// - connection exceptions (`08`), insufficient resources (`53`) and operator intervention (`57P01` to `57P03`):
///   `unavailable`.
///
/// `None` for anything else: a fault, `internal`.
#[must_use]
pub fn meaning_of(code: &str) -> Option<Meaning> {
    if let Some((_, meaning)) = LEDGER_SQLSTATES.iter().find(|(c, _)| *c == code) {
        return Some(*meaning);
    }
    match code {
        "22003" => Some(refused(Rule::Overflow)),
        "40001" | "40P01" | "57P01" | "57P02" | "57P03" => {
            Some(Meaning::Fault(ErrorClass::Unavailable))
        }
        _ if code.starts_with("08") || code.starts_with("53") => {
            Some(Meaning::Fault(ErrorClass::Unavailable))
        }
        _ => None,
    }
}

/// The events a DETAIL names. An id that is not a valid [`EventId`] is left out; the SQL only names ids it stored or
/// was given, which passed the same rule.
fn named_events(detail: Option<&str>) -> Vec<EventId> {
    detail
        .unwrap_or_default()
        .split(' ')
        .filter(|id| !id.is_empty())
        .filter_map(|id| EventId::new(id).ok())
        .collect()
}

/// A driver error as the port's error.
pub(crate) fn ledger_error(error: sqlx::Error) -> LedgerError {
    if let sqlx::Error::Database(database) = &error {
        let code = database.code().map(|c| c.into_owned()).unwrap_or_default();
        // A row for a day with no partition is a check violation with no constraint: the partition job is behind
        // (ADR 0001, point 10).
        if code == "23514" && database.constraint().is_none() {
            return store(ErrorClass::Unavailable, error);
        }
        match meaning_of(&code) {
            Some(Meaning::Refused { rule, names_events }) => {
                let refusal = if names_events {
                    let detail = database
                        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                        .and_then(sqlx::postgres::PgDatabaseError::detail);
                    Refusal::of_events(rule, named_events(detail))
                } else {
                    Refusal::new(rule)
                };
                return LedgerError::Refused(refusal);
            }
            Some(Meaning::Fault(class)) => return store(class, error),
            None => {}
        }
    }
    store(crate::class_of(&error), error)
}

fn store(class: ErrorClass, error: sqlx::Error) -> LedgerError {
    LedgerError::Store {
        class,
        source: Box::new(crate::Error::Query(error)),
    }
}

/// An answer from the database that the port's types refuse: a fault of the store.
pub(crate) fn corrupt() -> LedgerError {
    LedgerError::Store {
        class: ErrorClass::Internal,
        source: Box::new(crate::Error::UnexpectedAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openbilling_core::Classify;

    /// Each SQLSTATE the migrations raise, with what it must mean. A second copy, written by hand: changing the table
    /// without this test fails it.
    #[test]
    fn every_ledger_sqlstate_maps_to_its_rule_or_class() {
        let expected = [
            ("OB001", Rule::CreditConflict, false),
            ("OB002", Rule::UnknownAccount, false),
            ("OB003", Rule::UnknownCurrency, false),
            ("OB004", Rule::ReversalCap, false),
            ("OB005", Rule::IncompleteConfig, false),
            ("OB006", Rule::UnknownLabel, false),
            ("OB007", Rule::UnknownPayment, false),
            ("OB008", Rule::AccountConflict, false),
            ("OB009", Rule::InvalidAmount, false),
            ("OB010", Rule::InvalidConfig, false),
            ("OB101", Rule::TooManyEvents, false),
            ("OB102", Rule::NegativeAmount, true),
            ("OB103", Rule::DifferingDuplicate, true),
            ("OB104", Rule::PayloadConflict, true),
            ("OB105", Rule::TooOld, true),
            ("OB106", Rule::TooFarAhead, true),
            ("OB107", Rule::UnknownCurrency, true),
            ("OB108", Rule::Overflow, true),
            ("22003", Rule::Overflow, false),
        ];
        for (code, rule, names_events) in expected {
            assert_eq!(
                meaning_of(code),
                Some(Meaning::Refused { rule, names_events }),
                "{code}"
            );
        }
        for code in ["OB900", "OB901", "OB902"] {
            assert_eq!(
                meaning_of(code),
                Some(Meaning::Fault(ErrorClass::Internal)),
                "{code}"
            );
        }
        for code in ["40001", "40P01", "08006", "53300", "57P01"] {
            assert_eq!(
                meaning_of(code),
                Some(Meaning::Fault(ErrorClass::Unavailable)),
                "{code}"
            );
        }
        for code in ["23505", "42P01", "XX000", "OB999"] {
            assert_eq!(meaning_of(code), None, "{code}");
        }
    }

    #[test]
    fn every_rule_has_a_sqlstate_and_conflicts_are_conflicts() {
        for rule in Rule::ALL {
            let mapped = LEDGER_SQLSTATES
                .iter()
                .any(|(_, m)| matches!(m, Meaning::Refused { rule: r, .. } if *r == rule));
            assert!(mapped, "{rule:?} has no SQLSTATE");
        }
        for (code, meaning) in LEDGER_SQLSTATES {
            assert_eq!(code.len(), 5, "{code}");
            assert!(code.starts_with("OB"), "{code}");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "{code}"
            );
            if let Meaning::Refused { rule, .. } = meaning {
                assert_eq!(
                    rule.class(),
                    match rule {
                        Rule::PayloadConflict
                        | Rule::AccountConflict
                        | Rule::CreditConflict
                        | Rule::ReversalCap => ErrorClass::Conflict,
                        _ => ErrorClass::Invalid,
                    }
                );
            }
        }
    }

    /// Every `ERRCODE = 'OB…'` the migrations raise is in the table, and every code of the table is raised somewhere.
    #[test]
    fn the_table_and_the_migrations_name_the_same_codes() {
        let sql = crate::MIGRATIONS
            .iter()
            .map(|(_, _, sql)| *sql)
            .collect::<Vec<_>>()
            .join("\n");
        let mut raised: Vec<&str> = sql
            .match_indices("ERRCODE = 'OB")
            .map(|(at, _)| &sql[at + 11..at + 16])
            .collect();
        raised.sort_unstable();
        raised.dedup();
        let mut table: Vec<&str> = LEDGER_SQLSTATES.iter().map(|(code, _)| *code).collect();
        table.sort_unstable();
        assert_eq!(raised, table);
    }

    #[test]
    fn a_detail_names_its_events_and_nothing_else() {
        let names: Vec<_> = named_events(Some("e1 big-2 x\"y"))
            .iter()
            .map(|e| e.as_str().to_owned())
            .collect();
        assert_eq!(names, ["e1", "big-2", "x\"y"]);
        assert!(named_events(None).is_empty());
        assert!(named_events(Some("")).is_empty());
    }
}
