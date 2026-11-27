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
| `sums_that_overflow_are_refused_and_a_debt_never_overflows` | An owner's sum past the largest amount refuses the batch naming its events; a debit that would overflow the debt is `limit` |
| `a_repeat_counts_once_and_a_differing_payload_is_refused` | A differing copy in a batch, or a stored id with another payload, refuses the batch naming it |
| `the_same_id_on_another_day_is_refused_as_id_reused` | The first received wins, in one drain or a later one |
| `an_owner_without_an_account_in_the_currency_is_refused` | `unknown_account`, and nothing moves |
| `a_debit_past_the_debt_limit_is_refused` | `limit` when the debt would pass the limit; none means unlimited |
| `the_allowance_is_granted_once_per_account_and_month` | Once per account and month; another month is not spendable now; it repays debt first |
| `adjustments_add_and_take_credit` | Positive opens a lot; negative takes from its account's lots in order, the rest is debt; both once-only |
| `accounts_are_once_only_and_one_per_owner_currency_and_label` | Replay, conflicts, unknown currency and label, a credit to no account |
| `daily_usage_sums_applied_debits_per_utc_day` | Per UTC day of occurrence; refused events not counted |
| `invalid_amounts_and_configurations_are_refused` | Wrong signs per source and kind, malformed configurations, more than 1 000 events |

`fixtures` holds the helpers the cases share; a store's own tests may use them too.

## Tests

The suite runs where an implementation runs it: `openbilling-pg`'s `tests/ledger.rs`, which needs a PostgreSQL 19. The
crate has no tests of its own; there is no second implementation of the rules to run it against (agent guide,
rule 12).
