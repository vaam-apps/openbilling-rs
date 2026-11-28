# openbilling-testkit

The conformance suite of the storage port, [`LedgerStore`](../openbilling-core/README.md). An implementation runs every
case from one of its test files with one line:

```rust
openbilling_testkit::ledger_conformance!(support::fresh_ledger);
```

`fresh_ledger` is an `async fn() -> Option<S>` that returns a fresh, empty store per case, or `None` to skip it (a
store that needs a database it was not given). Each case is a `#[tokio::test]` on the multi-threaded runtime, so the
consumer needs `tokio` as a dev-dependency with `macros` and `rt-multi-thread`. The cases drive the store only through
the port, with the store's own clock, and assume the default window (seven days, five minutes).

## The cases

| Case | Holds |
|---|---|
| `the_same_batch_twice_debits_once` | A replay changes nothing, before and after the drain; a mixed batch debits only the new id; a repeat in a batch counts once |
| `a_bad_event_refuses_the_whole_batch_and_writes_nothing` | A negative amount or an unknown currency refuses the batch, names the event, and writes nothing |
| `concurrent_intake_and_drains_sum_exactly` | 20 overlapping intakes at once, then 4 drains at once: every id applied once, the sums exact |
| `concurrent_drains_with_scarce_credit_split_credit_and_debt_exactly` | 8 drains at once past the credit: the lot ends at 0 and the debt is exactly the rest |
| `concurrent_drains_over_many_owners_neither_deadlock_nor_lose_money` | Overlapping ids over three owners, drained in small batches while payments arrive: no deadlock, nothing lost |
| `a_payment_credits_once` | Retries one after another and at once book once; another amount or account is a conflict |
| `a_payment_id_credits_once_by_either_route` | By account id or by owner, currency and label, one payment id credits once; ids are once-only within a source |
| `sources_reversals_and_debt_repayment` | A promotion opens a promotion lot; refunds, replays, an unknown payment; a chargeback after spend leaves debt; the next payment repays it |
| `reversals_are_capped_at_the_payment` | Refunds and chargebacks together never pass the payment; a refused one leaves nothing; a partial refund after spend is debt |
| `concurrent_reversals_never_pass_the_cap` | Eight refunds of 300 at once against 1 000: exactly three pass |
| `a_credit_replay_must_match_its_terms` | A replay must match the expiry, the amount and the payment reversed; refunds and chargebacks have ids of their own |
| `the_debit_draws_in_the_configured_order` | Allowance, promotions by soonest expiry, payments oldest first, then debt; across accounts, the label order first |
| `the_draw_order_is_configuration` | A new label and lot order is followed; a configuration may not drop a label |
| `allowed_refuses_at_zero_or_below` | Zero and below are refused, above is allowed; an unknown owner reads zero; an expired lot is not spendable |
| `the_member_cap_is_per_owner_currency_member_and_month` | The cap counts a member's applied usage in a UTC month, per currency; it can be removed |
| `an_event_outside_the_window_refuses_the_batch_naming_it` | Too old and too far ahead refuse the batch naming only that event; just inside both edges is accepted |
| `sums_that_overflow_are_refused_and_a_debt_never_overflows` | An owner's sum past the largest amount refuses the batch naming its events; a debit that would overflow the debt is `overflow` |
| `a_repeat_counts_once_and_a_differing_payload_is_refused` | A differing copy in a batch, or a stored id with another payload, refuses the batch naming it |
| `the_same_id_on_another_day_is_refused_as_id_reused` | The first received wins, in one drain or a later one |
| `an_owner_without_an_account_in_the_currency_is_refused` | `unknown_account`, and nothing moves |
| `a_debit_past_the_debt_limit_is_booked_as_debt_and_flags_the_owner` | "Always book as debt": every debit applied, the debt past the limit; `allowed` answers `debt_limit` at or past it under the default policy, `balance` under it; credit repays and lifts it; none means unlimited |
| `allowed_compares_as_the_policy_says` | Each check (debt limit, balance, member cap) under `at_or_above`, `above` and `never` at its boundaries: a bound of zero with nothing used, used equal to the bound, one over; the checks in their order; the numbers the same whatever the policy (the owner's decision of 2026-11-27, ADR 0001, amended, F1 to F3) |
| `the_allowance_is_granted_once_per_account_and_month` | Once per account and month; another month is not spendable now; it repays debt first |
| `adjustments_add_and_take_credit` | Positive opens a lot; negative takes from its account's lots in order, the rest is debt; both once-only |
| `accounts_are_once_only_and_one_per_owner_currency_and_label` | Replay, conflicts, unknown currency and label, a credit to no account |
| `daily_usage_sums_applied_debits_per_utc_day` | Per UTC day of occurrence; a refused event (`id_reused`) not counted |
| `invalid_amounts_and_configurations_are_refused` | Wrong signs per source and kind, malformed configurations, more than 1 000 events |

## The price book's cases

A store that implements [`PriceBook`](../openbilling-core/README.md) also runs
`openbilling_testkit::price_book_conformance!(support::fresh_ledger);`
([ADR 0004](../../docs/adr/0004-a-versioned-price-book-in-sql.md)). The meters and prices are made up: an example AI
service with `tokens.input`, `tokens.output` and `tokens.cached` per model, and `compute.seconds`. Versions take effect
a minute or two after the store's clock, and the events rated occur between them, inside the default skew of five
minutes.

| Case | Holds |
|---|---|
| `the_same_event_rated_twice_gives_the_same_amount_and_version` | Rated twice, before and after its drain: one amount and one version; taken in, debited once, its outcome names the version |
| `a_new_version_rates_only_later_events` | An event before the new version keeps the old price; one at its instant and after has the new; the versions list in order |
| `an_unpriced_meter_or_dimension_is_refused` | An unknown meter, an unpriced currency, no version yet, an unpriced value, a missing or extra key: refused naming the event, by `rate` too; nothing written |
| `rounding_is_half_to_even_once_per_event` | Exact halves to the even neighbour, the rest to the nearest; two half micro-units are two zeros, not one |
| `a_huge_quantity_is_exact_or_refused` | `i64::MAX` tokens rated exactly; past the largest amount refused, alone or as a sum; zero costs nothing |
| `a_malformed_price_list_names_its_error` | A syntax error and an unknown key at the parser's line; each SQL check at its line and key; nothing loaded |
| `loading_the_same_list_twice_is_a_no_op` | The same content, however written, is one version; another price or time is another |
| `a_version_is_never_back_dated` | Not before now, not at or before an event already rated, not at another version's instant |
| `the_payload_of_a_raw_event_is_its_meter_quantity_and_dimensions` | Another quantity, meter or dimension, or a priced event with the same id and day, is a conflict; priced and raw events in one ledger |

`fixtures` holds the helpers the cases share, and `price_cases` the price book's (`example_list`, `raw`); a store's
own tests may use them too.

## The partition retention suite

`retention` is the conformance suite of `PartitionRetention` (ADR 0001, point 10, amended 2026-11-28):
`retention_conformance!(factory)`, where `factory` is an `async fn() -> Option<F>` making a fresh `RetentionFixture`: the
store under test and a way to plant a record in its queue as an intake long ago would have left it (intake refuses an
event older than the window, so no port can age one), and to list the days that have a partition. Cases count days
before the store's clock; a margin of `m` keeps every day on or after `today - (window + m)`.

| Case | Holds |
|---|---|
| `an_event_replayed_inside_the_window_is_still_refused_after_a_drop` | After an old partition is dropped, a replay of an event inside the window is still a replay and moves nothing; one too old for the window is refused `too_old`; an id applied in the dropped partition is refused as `id_reused` by the outcome kept for good; a second run finds nothing |
| `a_partition_with_an_undecided_event_is_kept` | An event still `received` keeps its partition, however old; once decided, the next run drops it |
| `a_partition_inside_the_window_plus_the_margin_is_kept` | The cutoff day and every day inside it stay, the day before goes; a longer margin keeps what a shorter one drops |
| `the_window_and_the_current_partition_are_never_touched` | Even the shortest margin (one day) leaves today and every day of the window |
| `concurrent_runs_drop_each_partition_once` | Four runs at once: every expired partition is dropped exactly once, a run that finds another working does nothing |

## The payment provider suite

`payments` is the conformance suite of `PaymentProvider` ([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)):
`payment_conformance!(factory)`, where `factory` is an `async fn() -> F` making a fresh `ProviderFixture`: the adapter
under test and a stand-in for its provider that can settle, cancel and refund a payment and sign webhooks as the
provider does. `openbilling-vpay`'s stand-in is the first.

| Case | Holds |
|---|---|
| `a_payment_is_created_once_per_key_and_read_back_pending` | One payment per key; read back pending with its amount, currency and target |
| `a_paid_payment_reads_succeeded_and_a_canceled_one_canceled` | The provider's word decides the status |
| `unknown_objects_and_inexact_amounts_are_refused` | `not_found` for a payment or a refund it does not know; `inexact_amount` before anything is asked |
| `a_foreign_payment_has_no_target` | A payment openbilling did not ask for credits nothing |
| `a_refund_is_read_back_with_its_payment_and_status` | All four refund statuses |
| `a_signed_webhook_names_its_object_and_nothing_else` | A payment, a refund, or nothing |
| `a_forged_stale_or_tampered_webhook_is_refused` | Forged, tampered, unsigned, outside the tolerance either way; the body read only after the signature |

## Tests

The suites run where an implementation runs them: `openbilling-pg`'s `tests/ledger.rs`, `tests/price_book.rs` and
`tests/retention.rs`, which need a PostgreSQL 19, and `openbilling-vpay`'s `tests/conformance.rs` for the payment suite, which needs
nothing. The
crate has no tests of its own; there is no second implementation of the rules to run it against (agent guide,
rule 12).
