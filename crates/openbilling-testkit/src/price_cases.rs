//! The conformance cases of the price book (ADR 0004). Each takes a fresh, empty store and drives it through the
//! [`PriceBook`] and [`LedgerStore`](openbilling_core::LedgerStore) ports alone.
//!
//! The meters and prices are made up: an example AI service that meters `tokens.input`, `tokens.output` and
//! `tokens.cached` per model, and `compute.seconds`. A version may take effect only after the store's clock, and an
//! event may occur at most five minutes after it (the default window), so the cases load versions a minute or two
//! ahead and rate events that occur between them.

use openbilling_core::{
    Amount, EventOutcome, Intake, LedgerError, PriceBook, Quantity, RawEvent, Rule, Timestamp,
};

use crate::fixtures::{
    MINUTE, drain_all, eur, event_id, micros, now, open, pay, plus, refusal_of, remaining, setup,
};

/// One second, in microseconds.
const SECOND: i64 = 1_000_000;

/// The example price list, taking effect at `at`. Lines: `effective_from` is line 2; the meters start at lines 4
/// (`tokens.input`), 7, 10 and 13 (`compute.seconds`); the prices at lines 15, 20, 25, 30 and 35.
pub fn example_list(at: Timestamp, input_micros: i64) -> String {
    format!(
        "\
# A made-up price list of an example AI service.
effective_from: \"{at}\"
meters:
  - name: tokens.input
    unit: token
    dimensions: [model]
  - name: tokens.output
    unit: token
    dimensions: [model]
  - name: tokens.cached
    unit: token
    dimensions: [model]
  - name: compute.seconds
prices:
  - meter: tokens.input
    currency: EUR
    dimensions: {{ model: example-small }}
    price_micros: {input_micros}
    per: 1000000
  - meter: tokens.input
    currency: EUR
    dimensions: {{ model: example-large }}
    price_micros: 2000
    per: 1000000
  - meter: tokens.output
    currency: EUR
    dimensions: {{ model: example-small }}
    price_micros: 600
    per: 1000000
  - meter: tokens.cached
    currency: EUR
    dimensions: {{ model: example-small }}
    price_micros: 0
    per: 1000000
  - meter: compute.seconds
    currency: EUR
    price_micros: 40
",
        at = at.to_rfc3339_micros()
    )
}

/// A raw EUR event of owner `o1` by member `m1`.
pub fn raw(
    id: &str,
    meter: &str,
    quantity: i64,
    dimensions: &[(&str, &str)],
    at: Timestamp,
) -> RawEvent {
    RawEvent {
        id: event_id(id),
        owner: openbilling_core::OwnerId::new("o1").expect("an owner"),
        currency: eur(),
        member: openbilling_core::MemberId::new("m1").expect("a member"),
        meter: openbilling_core::MeterId::new(meter).expect("a meter"),
        quantity: Quantity::new(quantity).expect("a quantity"),
        dimensions: dimensions
            .iter()
            .map(|(k, v)| {
                (
                    openbilling_core::DimensionKey::new(*k).expect("a key"),
                    openbilling_core::DimensionValue::new(*v).expect("a value"),
                )
            })
            .collect(),
        occurred_at: at,
    }
}

/// A small-model input event.
fn small(id: &str, tokens: i64, at: Timestamp) -> RawEvent {
    raw(
        id,
        "tokens.input",
        tokens,
        &[("model", "example-small")],
        at,
    )
}

/// The refusal's rule, line and key, failing on anything else.
fn refused_at<T: std::fmt::Debug>(result: Result<T, LedgerError>) -> (Rule, u32, Option<String>) {
    match result {
        Err(LedgerError::Refused(refusal)) => {
            let at = refusal.position().expect("a position");
            (refusal.rule(), at.line(), at.key().map(str::to_owned))
        }
        other => panic!("expected a refusal of the list, got {other:?}"),
    }
}

/// Configures the store, opens `a1` for `o1` in EUR with 1 000 000 paid in, and loads the example list at a minute
/// from now with an input price of 150 per million tokens. Returns the clock it read.
async fn with_a_version<S: PriceBook>(store: &S) -> (Timestamp, openbilling_core::Loaded) {
    setup(store).await;
    open(store, "a1", "o1", eur(), "main").await;
    pay(store, "p1", "a1", 1_000_000).await;
    let t = now(store).await;
    let loaded = store
        .load_price_list(&example_list(plus(t, MINUTE), 150))
        .await
        .expect("a version");
    (t, loaded)
}

/// The same event rated twice gives the same amount and version; taken in, it is debited that amount once, and its
/// outcome names the version.
pub async fn the_same_event_rated_twice_gives_the_same_amount_and_version<S: PriceBook>(store: S) {
    let (t, v1) = with_a_version(&store).await;
    assert!(!v1.replayed);
    let e = small("e1", 1_234_567, plus(t, 2 * MINUTE));
    let first = store.rate(&e).await.expect("rated");
    let second = store.rate(&e).await.expect("rated");
    assert_eq!(first, second);
    // 1 234 567 tokens at 150 per million: 185.185, rounded to 185.
    assert_eq!(first.amount, micros(185));
    assert_eq!(first.version, v1.version);

    let report = store
        .intake_raw(&[e.clone(), e.clone()])
        .await
        .expect("intake");
    assert_eq!(
        report.answers.iter().map(|(_, a)| *a).collect::<Vec<_>>(),
        [Intake::Accepted, Intake::Replayed]
    );
    assert_eq!(
        store
            .intake_raw(std::slice::from_ref(&e))
            .await
            .expect("a replay")
            .replayed(),
        1
    );
    let drained = drain_all(&store, 10).await;
    assert_eq!(drained.decided.len(), 1);
    assert_eq!(drained.decided[0].amount, micros(185));
    assert_eq!(drained.decided[0].price_version.as_ref(), Some(&v1.version));
    let outcome = store
        .outcome(&event_id("e1"))
        .await
        .expect("outcome")
        .expect("decided");
    assert_eq!(
        (outcome.outcome, outcome.amount),
        (EventOutcome::Applied, micros(185))
    );
    assert_eq!(outcome.price_version, Some(v1.version.clone()));
    assert_eq!(remaining(&store, "a1").await, 1_000_000 - 185);
    // Rated again after the drain: the same.
    assert_eq!(store.rate(&e).await.expect("rated"), first);
}

/// A new version rates only the events that occur from its time on; an event before it keeps the old price,
/// whenever it is rated.
pub async fn a_new_version_rates_only_later_events<S: PriceBook>(store: S) {
    let (t, v1) = with_a_version(&store).await;
    let v2 = store
        .load_price_list(&example_list(plus(t, 2 * MINUTE), 300))
        .await
        .expect("a second version");
    assert_ne!(v1.version, v2.version);
    assert_ne!(v1.content_hash, v2.content_hash);
    let versions = store.price_versions().await.expect("versions");
    assert_eq!(
        versions
            .iter()
            .map(|v| v.version.clone())
            .collect::<Vec<_>>(),
        [v1.version.clone(), v2.version.clone()]
    );
    assert_eq!(versions[0].effective_from, plus(t, MINUTE));

    let before = small("before", 1_000_000, plus(t, 90 * SECOND));
    let at_the_edge = small("edge", 1_000_000, plus(t, 2 * MINUTE));
    let after = small("after", 1_000_000, plus(t, 3 * MINUTE));
    let rated: Vec<_> = [&before, &at_the_edge, &after]
        .into_iter()
        .map(|e| (e.id.as_str().to_owned(), e))
        .collect();
    let mut answers = Vec::new();
    for (id, e) in &rated {
        let r = store.rate(e).await.expect("rated");
        answers.push((id.clone(), r.amount.micros(), r.version));
    }
    assert_eq!(
        answers,
        [
            ("before".to_owned(), 150, v1.version.clone()),
            ("edge".to_owned(), 300, v2.version.clone()),
            ("after".to_owned(), 300, v2.version.clone()),
        ]
    );
    store
        .intake_raw(&[before.clone(), at_the_edge, after])
        .await
        .expect("intake");
    let drained = drain_all(&store, 10).await;
    assert_eq!(drained.debited(), micros(750));
    let kept = store
        .outcome(&event_id("before"))
        .await
        .expect("outcome")
        .expect("decided");
    assert_eq!(
        (kept.amount, kept.price_version),
        (micros(150), Some(v1.version))
    );
}

/// An unpriced meter or dimension is refused with its own rule, naming the event, and nothing is written; it is
/// never priced at a guess.
pub async fn an_unpriced_meter_or_dimension_is_refused<S: PriceBook>(store: S) {
    let (t, _) = with_a_version(&store).await;
    let at = plus(t, 2 * MINUTE);
    let good = small("good", 10, at);
    let cases = [
        (
            raw("x", "images.generated", 1, &[], at),
            Rule::UnpricedMeter,
        ),
        (
            RawEvent {
                currency: openbilling_core::CurrencyCode::new("USD").expect("usd"),
                ..small("x", 1, at)
            },
            Rule::UnpricedMeter,
        ),
        // Before any version is in force.
        (small("x", 1, plus(t, 30 * SECOND)), Rule::UnpricedMeter),
        (
            raw("x", "tokens.input", 1, &[("model", "example-huge")], at),
            Rule::UnpricedDimension,
        ),
        (
            raw("x", "tokens.input", 1, &[], at),
            Rule::UnpricedDimension,
        ),
        (
            raw(
                "x",
                "tokens.input",
                1,
                &[("model", "example-small"), ("region", "r1")],
                at,
            ),
            Rule::UnpricedDimension,
        ),
        (
            raw("x", "compute.seconds", 1, &[("model", "example-small")], at),
            Rule::UnpricedDimension,
        ),
    ];
    for (bad, rule) in cases {
        let (got, named) = refusal_of(store.intake_raw(&[good.clone(), bad.clone()]).await);
        assert_eq!((got, named), (rule, vec!["x".to_owned()]), "{bad:?}");
        assert_eq!(
            crate::fixtures::rule_of(store.rate(&bad).await),
            rule,
            "{bad:?}"
        );
    }
    // Nothing was written: `good` is accepted now, not replayed, and is the one event the drain sees.
    assert!(drain_all(&store, 10).await.decided.is_empty());
    assert_eq!(
        store.intake_raw(&[good]).await.expect("intake").accepted(),
        1
    );
    assert_eq!(drain_all(&store, 10).await.decided.len(), 1);
}

/// Rounding is to the micro-unit, half to even, once per event.
pub async fn rounding_is_half_to_even_once_per_event<S: PriceBook>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000_000).await;
    let t = now(&store).await;
    let list = format!(
        "\
effective_from: \"{}\"
meters:
  - name: units.half
  - name: units.odd
prices:
  - meter: units.half
    currency: EUR
    price_micros: 1
    per: 2
  - meter: units.odd
    currency: EUR
    price_micros: 3
    per: 10000
",
        plus(t, MINUTE).to_rfc3339_micros()
    );
    store.load_price_list(&list).await.expect("a version");
    let at = plus(t, 2 * MINUTE);
    // (meter, quantity, amount): an exact half goes to the even neighbour, anything else to the nearest.
    let expected = [
        ("units.half", 1, 0),
        ("units.half", 2, 1),
        ("units.half", 3, 2),
        ("units.half", 5, 2),
        ("units.half", 7, 4),
        ("units.odd", 1_666, 0),
        ("units.odd", 1_667, 1),
        ("units.odd", 5_000, 2),
        ("units.odd", 15_000, 4),
        ("units.odd", 25_000, 8),
    ];
    for (meter, quantity, amount) in expected {
        let rated = store
            .rate(&raw("x", meter, quantity, &[], at))
            .await
            .expect("rated");
        assert_eq!(rated.amount, micros(amount), "{meter} × {quantity}");
    }
    // Once per event: two events of half a micro-unit each are each zero, never one together.
    let halves = [
        raw("h1", "units.half", 1, &[], at),
        raw("h2", "units.half", 1, &[], at),
    ];
    store.intake_raw(&halves).await.expect("intake");
    let three = raw("h3", "units.half", 3, &[], at);
    store.intake_raw(&[three]).await.expect("intake");
    let drained = drain_all(&store, 10).await;
    assert_eq!(drained.decided.len(), 3);
    assert_eq!(drained.debited(), micros(2));
}

/// A huge quantity is rated exactly; an amount past the largest is refused, never wrapped or clamped.
pub async fn a_huge_quantity_is_exact_or_refused<S: PriceBook>(store: S) {
    let (t, _) = with_a_version(&store).await;
    let at = plus(t, 2 * MINUTE);
    // i64::MAX tokens at 150 per million: 1 383 505 805 528 216.2…, exact in numeric.
    let rated = store
        .rate(&small("big", i64::MAX, at))
        .await
        .expect("rated");
    assert_eq!(rated.amount, micros(1_383_505_805_528_216));
    // i64::MAX seconds at 40 each passes the largest amount.
    let past = raw("past", "compute.seconds", i64::MAX, &[], at);
    assert_eq!(
        crate::fixtures::rule_of(store.rate(&past).await),
        Rule::Overflow
    );
    let (rule, named) = refusal_of(store.intake_raw(&[small("fine", 1, at), past]).await);
    assert_eq!((rule, named), (Rule::Overflow, vec!["past".to_owned()]));
    // Two that fit alone but not together are refused as a sum, naming both.
    let half = i64::MAX / 40;
    let (rule, named) = refusal_of(
        store
            .intake_raw(&[
                raw("s1", "compute.seconds", half, &[], at),
                raw("s2", "compute.seconds", half, &[], at),
            ])
            .await,
    );
    assert_eq!(
        (rule, named),
        (Rule::Overflow, vec!["s1".to_owned(), "s2".to_owned()])
    );
    // Zero is a quantity, and costs nothing.
    let zero = store.rate(&small("zero", 0, at)).await.expect("rated");
    assert_eq!(zero.amount, Amount::ZERO);
    assert!(drain_all(&store, 10).await.decided.is_empty());
}

/// A malformed list is refused naming its line, and its key where the database checked it; nothing is loaded.
pub async fn a_malformed_price_list_names_its_error<S: PriceBook>(store: S) {
    setup(&store).await;
    let t = now(&store).await;
    let list = example_list(plus(t, MINUTE), 150);
    let cases = [
        // A syntax error, and a key the form does not have: the parser's line, no key.
        (
            list.replace(
                "[model]\n  - name: tokens.output",
                "[model\n  - name: tokens.output",
            ),
            Rule::MalformedPriceList,
            None,
            None,
        ),
        (
            list.replace(
                "    unit: token\n    dimensions: [model]\n  - name: tokens.output",
                "    unti: token\n    dimensions: [model]\n  - name: tokens.output",
            ),
            Rule::MalformedPriceList,
            Some(5),
            None,
        ),
        // The database's checks: the line of the entry and the key of the value.
        (
            list.replace("effective_from: \"", "effective_from: \"x"),
            Rule::InvalidPriceList,
            Some(2),
            Some("effective_from"),
        ),
        (
            list.replace("name: tokens.output", "name: Tokens.Output"),
            Rule::InvalidPriceList,
            Some(7),
            Some("meters[1].name"),
        ),
        (
            list.replace("name: tokens.cached", "name: tokens.output"),
            Rule::PriceListDuplicate,
            Some(10),
            Some("meters[2].name"),
        ),
        (
            list.replace("price_micros: 2000", "price_micros: -1"),
            Rule::InvalidPriceList,
            Some(20),
            Some("prices[1].price_micros"),
        ),
        (
            list.replace("price_micros: 40\n", "price_micros: 40\n    per: 0\n"),
            Rule::InvalidPriceList,
            Some(35),
            Some("prices[4].per"),
        ),
        (
            list.replace(
                "  - meter: tokens.output\n    currency: EUR",
                "  - meter: tokens.output\n    currency: GBP",
            ),
            Rule::UnknownCurrency,
            Some(25),
            Some("prices[2].currency"),
        ),
        (
            list.replace("  - meter: tokens.cached", "  - meter: tokens.unknown"),
            Rule::PriceListMismatch,
            Some(30),
            Some("prices[3].meter"),
        ),
        (
            list.replace(
                "    dimensions: { model: example-large }",
                "    dimensions: { size: example-large }",
            ),
            Rule::PriceListMismatch,
            Some(20),
            Some("prices[1].dimensions"),
        ),
        (
            list.replace("example-large", "example-small"),
            Rule::PriceListDuplicate,
            Some(20),
            Some("prices[1]"),
        ),
    ];
    for (yaml, rule, line, key) in cases {
        let (got, at, named) = refused_at(store.load_price_list(&yaml).await);
        assert_eq!(got, rule, "{yaml}");
        if let Some(line) = line {
            assert_eq!(at, line, "{yaml}");
        }
        assert_eq!(named.as_deref(), key, "{yaml}");
    }
    assert!(store.price_versions().await.expect("versions").is_empty());
    store
        .load_price_list(&list)
        .await
        .expect("the list itself loads");
}

/// Loading the same list twice is a no-op, keyed by the hash of its content: its layout, comments, order and the
/// spelling of its time do not count; its prices and its time do.
pub async fn loading_the_same_list_twice_is_a_no_op<S: PriceBook>(store: S) {
    setup(&store).await;
    let t = now(&store).await;
    let at = plus(t, MINUTE);
    let list = example_list(at, 150);
    let first = store.load_price_list(&list).await.expect("loaded");
    let again = store.load_price_list(&list).await.expect("loaded");
    assert!(again.replayed);
    assert_eq!(
        (&again.version, &again.content_hash),
        (&first.version, &first.content_hash)
    );
    assert_eq!(again.effective_from, at);
    assert_eq!(first.content_hash.len(), 64);

    // The same content, written otherwise: the prices in another order, flow style, no comment, `per` spelled out
    // where it was left out, and the time without its fraction when it has none.
    let seconds =
        Timestamp::from_unix_micros(at.unix_micros() - at.unix_micros() % SECOND).expect("a time");
    let whole = store
        .load_price_list(&example_list(plus(seconds, MINUTE), 150))
        .await
        .expect("loaded");
    let reworded = format!(
        "effective_from: \"{}\"\n\
         prices:\n\
         - {{meter: compute.seconds, currency: EUR, price_micros: 40, per: 1}}\n\
         - {{meter: tokens.cached, currency: EUR, dimensions: {{model: example-small}}, price_micros: 0, per: 1000000}}\n\
         - {{meter: tokens.output, currency: EUR, dimensions: {{model: example-small}}, price_micros: 600, per: 1000000}}\n\
         - {{meter: tokens.input, currency: EUR, dimensions: {{model: example-large}}, price_micros: 2000, per: 1000000}}\n\
         - {{meter: tokens.input, currency: EUR, dimensions: {{model: example-small}}, price_micros: 150, per: 1000000}}\n\
         meters:\n\
         - {{name: compute.seconds}}\n\
         - {{name: tokens.cached, unit: token, dimensions: [model]}}\n\
         - {{name: tokens.output, unit: token, dimensions: [model]}}\n\
         - {{name: tokens.input, unit: token, dimensions: [model]}}\n",
        plus(seconds, MINUTE)
            .to_rfc3339_micros()
            .replace(".000000Z", "Z")
    );
    let replayed = store.load_price_list(&reworded).await.expect("loaded");
    assert!(replayed.replayed, "{reworded}");
    assert_eq!(replayed.version, whole.version);
    assert_eq!(store.price_versions().await.expect("versions").len(), 2);

    // Another price is another version.
    let other = store
        .load_price_list(&example_list(plus(t, 3 * MINUTE), 151))
        .await
        .expect("loaded");
    assert!(!other.replayed);
    assert_eq!(store.price_versions().await.expect("versions").len(), 3);
}

/// A version is never back-dated: not before now, and not at or before an event already rated; and two versions
/// never take effect at one instant.
pub async fn a_version_is_never_back_dated<S: PriceBook>(store: S) {
    let (t, _) = with_a_version(&store).await;
    let (rule, line, key) = refused_at(
        store
            .load_price_list(&example_list(plus(t, -MINUTE), 1))
            .await,
    );
    assert_eq!(
        (rule, line, key.as_deref()),
        (Rule::BackDated, 2, Some("effective_from"))
    );
    let (rule, _, key) = refused_at(
        store
            .load_price_list(&example_list(plus(t, MINUTE), 1))
            .await,
    );
    assert_eq!(
        (rule, key.as_deref()),
        (Rule::VersionConflict, Some("effective_from"))
    );

    // An event rated by the first version three minutes from now: a version two minutes from now would be in force
    // where that event already has the first one's price.
    store
        .intake_raw(&[small("ahead", 1_000_000, plus(t, 3 * MINUTE))])
        .await
        .expect("intake");
    let (rule, _, _) = refused_at(
        store
            .load_price_list(&example_list(plus(t, 2 * MINUTE), 300))
            .await,
    );
    assert_eq!(rule, Rule::BackDated);
    let (rule, _, _) = refused_at(
        store
            .load_price_list(&example_list(plus(t, 3 * MINUTE), 300))
            .await,
    );
    assert_eq!(rule, Rule::BackDated);
    let later = store
        .load_price_list(&example_list(plus(t, 4 * MINUTE), 300))
        .await
        .expect("after every rated event");
    assert!(!later.replayed);
    assert_eq!(store.price_versions().await.expect("versions").len(), 2);
}

/// A raw event goes through the ledger's once-only intake: its payload hash covers the meter, the quantity and the
/// dimensions, and a raw and a priced event with one id and day differ.
pub async fn the_payload_of_a_raw_event_is_its_meter_quantity_and_dimensions<S: PriceBook>(
    store: S,
) {
    let (t, _) = with_a_version(&store).await;
    let at = plus(t, 2 * MINUTE);
    store
        .intake_raw(&[small("e1", 10, at)])
        .await
        .expect("intake");
    for other in [
        small("e1", 11, at),
        raw("e1", "tokens.output", 10, &[("model", "example-small")], at),
        raw("e1", "tokens.input", 10, &[("model", "example-large")], at),
    ] {
        let (rule, named) = refusal_of(store.intake_raw(&[other]).await);
        assert_eq!(
            (rule, named),
            (Rule::PayloadConflict, vec!["e1".to_owned()])
        );
    }
    let (rule, _) = refusal_of(
        store
            .intake(&[crate::fixtures::event("e1", "o1", "m1", 2, at)])
            .await,
    );
    assert_eq!(rule, Rule::PayloadConflict);
    let (rule, named) = refusal_of(
        store
            .intake_raw(&[small("e2", 1, at), small("e2", 2, at)])
            .await,
    );
    assert_eq!(
        (rule, named),
        (Rule::DifferingDuplicate, vec!["e2".to_owned()])
    );
    assert_eq!(
        store
            .intake_raw(&[small("e1", 10, at)])
            .await
            .expect("a replay")
            .replayed(),
        1
    );
    // A priced event and a raw one in the same ledger, each debited once.
    store
        .intake(&[crate::fixtures::event("p", "o1", "m1", 7, t)])
        .await
        .expect("a priced event");
    let drained = drain_all(&store, 10).await;
    // Ten input tokens at 150 per million round to zero; the priced event is 7.
    assert_eq!(drained.decided.len(), 2);
    assert_eq!(drained.debited(), micros(7));
    let priced = store
        .outcome(&event_id("p"))
        .await
        .expect("outcome")
        .expect("decided");
    assert_eq!(priced.price_version, None);
}
