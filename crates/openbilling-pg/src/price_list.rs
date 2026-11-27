//! The YAML price list, parsed into the arrays `ob_load_price_list` checks (ADR 0004).
//!
//! Parsing is the one thing Rust does with a list: it reads the YAML, refuses what does not parse as a price list
//! (a syntax error, a missing or unknown key, a value of the wrong type, a key given twice) as
//! [`Rule::MalformedPriceList`] at the line and column the parser names, and passes every value, with the line and
//! column it came from, to the database. Every rule about the values (names, currencies, signs, duplicates, the
//! dimensions of a meter, the time) is the SQL's (agent guide, rule 12).
//!
//! The form (every key but `unit`, `dimensions` and `per` is required; no other key is allowed):
//!
//! ```yaml
//! effective_from: "2026-12-01T00:00:00Z"
//! meters:
//!   - name: tokens.input
//!     unit: token
//!     dimensions: [model]
//! prices:
//!   - meter: tokens.input
//!     currency: EUR
//!     dimensions: { model: example-small }
//!     price_micros: 150
//!     per: 1000000
//! ```
//!
//! The parser's own message is never kept: it may quote the list, and an error never holds the value it refused
//! (agent guide, rule 9).

use std::collections::BTreeMap;

use openbilling_core::{LedgerError, ListPosition, Refusal, Rule};
use serde::Deserialize;
use serde_saphyr::{Location, Spanned};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    effective_from: Spanned<String>,
    meters: Spanned<Vec<Spanned<Meter>>>,
    prices: Spanned<Vec<Spanned<Price>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meter {
    name: String,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    dimensions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Price {
    meter: String,
    currency: String,
    #[serde(default)]
    dimensions: BTreeMap<String, String>,
    price_micros: i64,
    #[serde(default)]
    per: Option<i64>,
}

/// A parsed list, as the arguments of `ob_load_price_list`. Positions are 1-based; lines and columns are 0 when the
/// parser did not know them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ParsedList {
    pub effective_from: String,
    /// The line and column of `effective_from`, `meters` and `prices`.
    pub top: Vec<i32>,
    pub meter_names: Vec<String>,
    pub meter_units: Vec<Option<String>>,
    pub meter_lines: Vec<i32>,
    pub meter_columns: Vec<i32>,
    pub meter_dimension_of: Vec<i32>,
    pub meter_dimensions: Vec<String>,
    pub price_meters: Vec<String>,
    pub price_currencies: Vec<String>,
    pub price_micros: Vec<i64>,
    pub price_per: Vec<Option<i64>>,
    pub price_lines: Vec<i32>,
    pub price_columns: Vec<i32>,
    pub price_dimension_of: Vec<i32>,
    pub price_dimension_keys: Vec<String>,
    pub price_dimension_values: Vec<String>,
}

fn coordinate(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn line_and_column(at: Location) -> (i32, i32) {
    (coordinate(at.line()), coordinate(at.column()))
}

fn position_of(count: usize) -> i32 {
    // At most what the database refuses: it takes 1000 meters and 100000 prices.
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// Parses `yaml` as a price list.
///
/// # Errors
///
/// [`Rule::MalformedPriceList`] at the parser's line and column, with no key, when it does not parse as one.
pub(crate) fn parse(yaml: &str) -> Result<ParsedList, LedgerError> {
    let file: File = serde_saphyr::from_str(yaml).map_err(|error| {
        let (line, column) = error.location().map_or((0, 0), |at| {
            (
                u32::try_from(at.line()).unwrap_or(u32::MAX),
                u32::try_from(at.column()).unwrap_or(u32::MAX),
            )
        });
        LedgerError::Refused(Refusal::at(
            Rule::MalformedPriceList,
            ListPosition::at(line, column),
        ))
    })?;

    let mut list = ParsedList {
        effective_from: file.effective_from.value,
        ..ParsedList::default()
    };
    for at in [
        file.effective_from.referenced,
        file.meters.referenced,
        file.prices.referenced,
    ] {
        let (line, column) = line_and_column(at);
        list.top.extend([line, column]);
    }
    for (n, meter) in file.meters.value.into_iter().enumerate() {
        let (line, column) = line_and_column(meter.referenced);
        let meter = meter.value;
        list.meter_names.push(meter.name);
        list.meter_units.push(meter.unit);
        list.meter_lines.push(line);
        list.meter_columns.push(column);
        for dimension in meter.dimensions {
            list.meter_dimension_of.push(position_of(n + 1));
            list.meter_dimensions.push(dimension);
        }
    }
    for (n, price) in file.prices.value.into_iter().enumerate() {
        let (line, column) = line_and_column(price.referenced);
        let price = price.value;
        list.price_meters.push(price.meter);
        list.price_currencies.push(price.currency);
        list.price_micros.push(price.price_micros);
        list.price_per.push(price.per);
        list.price_lines.push(line);
        list.price_columns.push(column);
        for (key, value) in price.dimensions {
            list.price_dimension_of.push(position_of(n + 1));
            list.price_dimension_keys.push(key);
            list.price_dimension_values.push(value);
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
# A made-up price list.
effective_from: \"2030-01-01T00:00:00Z\"
meters:
  - name: tokens.input
    unit: token
    dimensions: [model]
  - name: compute.seconds
prices:
  - meter: tokens.input
    currency: EUR
    dimensions: { model: example-small }
    price_micros: 150
    per: 1000000
  - meter: compute.seconds
    currency: EUR
    price_micros: 40
";

    fn malformed_at(yaml: &str) -> (u32, u32) {
        match parse(yaml) {
            Err(LedgerError::Refused(refusal)) => {
                assert_eq!(refusal.rule(), Rule::MalformedPriceList);
                let at = refusal.position().expect("a position");
                assert_eq!(at.key(), None);
                (at.line(), at.column())
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_list_parses_with_its_lines() {
        let list = parse(LIST).expect("a list");
        assert_eq!(list.effective_from, "2030-01-01T00:00:00Z");
        assert_eq!(list.top[0], 2);
        assert_eq!(list.meter_names, ["tokens.input", "compute.seconds"]);
        assert_eq!(list.meter_units, [Some("token".to_owned()), None]);
        assert_eq!(list.meter_lines, [4, 7]);
        assert_eq!(
            (list.meter_dimension_of, list.meter_dimensions),
            (vec![1], vec!["model".to_owned()])
        );
        assert_eq!(list.price_meters, ["tokens.input", "compute.seconds"]);
        assert_eq!(list.price_micros, [150, 40]);
        assert_eq!(list.price_per, [Some(1_000_000), None]);
        assert_eq!(list.price_lines, [9, 14]);
        assert_eq!(list.price_dimension_of, [1]);
        assert_eq!(list.price_dimension_keys, ["model"]);
        assert_eq!(list.price_dimension_values, ["example-small"]);
    }

    #[test]
    fn what_does_not_parse_is_refused_at_its_line() {
        // A syntax error: an unclosed flow sequence.
        assert!(malformed_at(&LIST.replace("[model]", "[model")).0 >= 6);
        // An unknown key, a missing one, a wrong type, a key given twice.
        assert_eq!(
            malformed_at(&LIST.replace("    per: 1000000", "    pre: 1000000")).0,
            13
        );
        assert!(malformed_at(&LIST.replace("    price_micros: 40\n", "")).0 >= 14);
        assert_eq!(
            malformed_at(&LIST.replace("price_micros: 150", "price_micros: lots")).0,
            12
        );
        assert_eq!(
            malformed_at(&LIST.replace("{ model: example-small }", "{ model: a, model: b }")).0,
            11
        );
        // An empty document, and one that is not a mapping.
        assert!(malformed_at("").0 <= 1);
        assert_eq!(malformed_at("- just\n- a list\n").0, 1);
    }

    #[test]
    fn the_refusal_quotes_nothing_of_the_list() {
        let error = parse(&LIST.replace("price_micros: 150", "price_micros: secret-value"))
            .expect_err("a wrong type");
        assert!(!error.to_string().contains("secret"), "{error}");
        assert!(!format!("{error:?}").contains("secret"), "{error:?}");
    }
}
