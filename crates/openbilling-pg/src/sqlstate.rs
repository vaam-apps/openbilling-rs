//! What the ledger's SQL raises, mapped to a refusal or an error class: the one place it is mapped.
//!
//! The migrations raise SQLSTATEs of class `OB` (five upper-case letters and digits, outside the classes the SQL
//! standard reserves, which PL/pgSQL allows: *verified 2026-11-27*, PostgreSQL 19 documentation, 41.9 *Errors and
//! Messages*, and Appendix A). An `OB1xx` refusal names its events in the error's DETAIL, separated by one space; an
//! id holds no space, so the split is exact. An `OB2xx` refusal of a price list names its position in DETAIL as
//! `<line> <column> <key>`; a key is built from the list's structure and holds no space.

use openbilling_core::{ErrorClass, EventId, LedgerError, ListPosition, Refusal, Rule};

/// What a refusal's DETAIL names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    /// Nothing.
    Nothing,
    /// The events that broke the rule.
    Events,
    /// The position in a price list, when DETAIL holds one.
    Position,
}

/// What a SQLSTATE of the ledger means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meaning {
    /// A rule refused the request.
    Refused {
        /// The rule.
        rule: Rule,
        /// What DETAIL names.
        names: Names,
    },
    /// A fault of the store: an invariant the SQL holds was about to break.
    Fault(ErrorClass),
}

const fn refused(rule: Rule) -> Meaning {
    Meaning::Refused {
        rule,
        names: Names::Nothing,
    }
}

const fn refused_naming(rule: Rule) -> Meaning {
    Meaning::Refused {
        rule,
        names: Names::Events,
    }
}

const fn refused_at(rule: Rule) -> Meaning {
    Meaning::Refused {
        rule,
        names: Names::Position,
    }
}

/// Every SQLSTATE the migrations raise, and what it means.
pub const LEDGER_SQLSTATES: [(&str, Meaning); 33] = [
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
    ("OB011", refused(Rule::TopUpConflict)),
    ("OB101", refused(Rule::TooManyEvents)),
    ("OB102", refused_naming(Rule::NegativeAmount)),
    ("OB103", refused_naming(Rule::DifferingDuplicate)),
    ("OB104", refused_naming(Rule::PayloadConflict)),
    ("OB105", refused_naming(Rule::TooOld)),
    ("OB106", refused_naming(Rule::TooFarAhead)),
    ("OB107", refused_naming(Rule::UnknownCurrency)),
    ("OB108", refused_naming(Rule::Overflow)),
    ("OB109", refused_naming(Rule::NegativeQuantity)),
    ("OB110", refused_naming(Rule::UnpricedMeter)),
    ("OB111", refused_naming(Rule::UnpricedDimension)),
    ("OB201", refused_at(Rule::InvalidPriceList)),
    ("OB202", refused_at(Rule::PriceListDuplicate)),
    ("OB203", refused_at(Rule::PriceListMismatch)),
    ("OB204", refused_at(Rule::UnknownCurrency)),
    ("OB205", refused_at(Rule::BackDated)),
    ("OB206", refused_at(Rule::VersionConflict)),
    ("OB900", Meaning::Fault(ErrorClass::Internal)),
    ("OB901", Meaning::Fault(ErrorClass::Internal)),
    ("OB902", Meaning::Fault(ErrorClass::Internal)),
    ("OB903", Meaning::Fault(ErrorClass::Internal)),
    ("OB904", Meaning::Fault(ErrorClass::Internal)),
];

/// The rules no SQLSTATE raises: [`Rule::MalformedPriceList`], which the store's parser decides before anything
/// reaches the database.
pub const RULES_DECIDED_IN_RUST: [Rule; 1] = [Rule::MalformedPriceList];

/// What `code` means, among the ledger's own SQLSTATEs and the standard ones the ledger relies on:
///
/// - `22003` (`numeric_value_out_of_range`): a debt past the largest amount, refused as `overflow`;
/// - `40001` and `40P01` (serialization failure, deadlock), `55P03` (lock timeout) and `57014` (statement timeout): the
///   same call may succeed, `unavailable`;
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
        "40001" | "40P01" | "55P03" | "57014" | "57P01" | "57P02" | "57P03" => {
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

/// The position a DETAIL names: `<line> <column> <key>`. `None` when it does not hold one (a trigger's refusal of a
/// version written directly names none).
fn named_position(detail: Option<&str>) -> Option<ListPosition> {
    let mut parts = detail?.split(' ');
    let line = parts.next()?.parse().ok()?;
    let column = parts.next()?.parse().ok()?;
    let key = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    ListPosition::with_key(line, column, key)
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
            Some(Meaning::Refused { rule, names }) => {
                let detail = database
                    .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                    .and_then(sqlx::postgres::PgDatabaseError::detail);
                let refusal = match names {
                    Names::Nothing => Refusal::new(rule),
                    Names::Events => Refusal::of_events(rule, named_events(detail)),
                    Names::Position => named_position(detail)
                        .map_or_else(|| Refusal::new(rule), |at| Refusal::at(rule, at)),
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
        let (nothing, events, position) = (Names::Nothing, Names::Events, Names::Position);
        let expected = [
            ("OB001", Rule::CreditConflict, nothing),
            ("OB002", Rule::UnknownAccount, nothing),
            ("OB003", Rule::UnknownCurrency, nothing),
            ("OB004", Rule::ReversalCap, nothing),
            ("OB005", Rule::IncompleteConfig, nothing),
            ("OB006", Rule::UnknownLabel, nothing),
            ("OB007", Rule::UnknownPayment, nothing),
            ("OB008", Rule::AccountConflict, nothing),
            ("OB009", Rule::InvalidAmount, nothing),
            ("OB010", Rule::InvalidConfig, nothing),
            ("OB011", Rule::TopUpConflict, nothing),
            ("OB101", Rule::TooManyEvents, nothing),
            ("OB102", Rule::NegativeAmount, events),
            ("OB103", Rule::DifferingDuplicate, events),
            ("OB104", Rule::PayloadConflict, events),
            ("OB105", Rule::TooOld, events),
            ("OB106", Rule::TooFarAhead, events),
            ("OB107", Rule::UnknownCurrency, events),
            ("OB108", Rule::Overflow, events),
            ("OB109", Rule::NegativeQuantity, events),
            ("OB110", Rule::UnpricedMeter, events),
            ("OB111", Rule::UnpricedDimension, events),
            ("OB201", Rule::InvalidPriceList, position),
            ("OB202", Rule::PriceListDuplicate, position),
            ("OB203", Rule::PriceListMismatch, position),
            ("OB204", Rule::UnknownCurrency, position),
            ("OB205", Rule::BackDated, position),
            ("OB206", Rule::VersionConflict, position),
            ("22003", Rule::Overflow, nothing),
        ];
        for (code, rule, names) in expected {
            assert_eq!(
                meaning_of(code),
                Some(Meaning::Refused { rule, names }),
                "{code}"
            );
        }
        for code in ["OB900", "OB901", "OB902", "OB903", "OB904"] {
            assert_eq!(
                meaning_of(code),
                Some(Meaning::Fault(ErrorClass::Internal)),
                "{code}"
            );
        }
        for code in [
            "40001", "40P01", "55P03", "57014", "08006", "53300", "57P01",
        ] {
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
            assert_eq!(
                mapped,
                !RULES_DECIDED_IN_RUST.contains(&rule),
                "{rule:?}: a SQLSTATE, unless Rust decides it"
            );
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
                        | Rule::ReversalCap
                        | Rule::TopUpConflict
                        | Rule::VersionConflict => ErrorClass::Conflict,
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
    fn a_detail_names_its_position_or_nothing() {
        let at = named_position(Some("12 5 prices[2].currency")).expect("a position");
        assert_eq!(
            (at.line(), at.column(), at.key()),
            (12, 5, Some("prices[2].currency"))
        );
        for bad in [
            None,
            Some(""),
            Some("12 5"),
            Some("x 5 prices"),
            Some("1 2 a b"),
            Some("1 2 Prices"),
        ] {
            assert_eq!(named_position(bad), None, "{bad:?}");
        }
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
