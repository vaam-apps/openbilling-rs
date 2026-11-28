//! Conformance tests for the storage port of openbilling-rs, [`LedgerStore`](openbilling_core::LedgerStore).
//!
//! The agent guide (rule 4) makes storage a trait with a conformance testkit: one set of tests, written once, that
//! every implementation must pass. An implementation runs the whole suite from one of its test files with
//! [`ledger_conformance!`], naming an async function that returns a fresh, empty store, or `None` to skip (a store
//! that needs a database it was not given).
//!
//! The cases drive the store only through the port. They hold the promises of
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md) and
//! [ADR 0002](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0002-an-owner-holds-several-accounts.md):
//! once-only intake, replays and conflicts, the window, the drain, the draw order across an owner's accounts and
//! their lots, credits by source, the reversal cap, debt, limits and caps, and concurrency with scarce credit.
//!
//! [`price_book_conformance!`] runs the cases of the price book
//! ([ADR 0004](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0004-a-versioned-price-book-in-sql.md)):
//! loading, versions, rating, rounding, refusals and raw intake.
//!
//! [`retention_conformance!`] runs the cases of partition retention ([`retention`], ADR 0001, point 10): a store also
//! gives them a [`retention::RetentionFixture`], because no port can age a record.
//!
//! [`top_up_conformance!`] runs the cases of top-ups ([`top_up`], ADR 0003 amended 2026-11-28): recorded once, a payment
//! read back compared with its top-up, the credit booked from the recorded terms once, and the pending ones claimed
//! for reconciliation once.
//!
//! [`payment_conformance!`] does the same for a payment provider adapter ([`payments`], ADR 0003), against a
//! stand-in for the provider that the adapter reaches over the network.
//!
//! The consumer needs `tokio` as a dev-dependency with the `macros` and `rt-multi-thread` features: each case is a
//! `#[tokio::test]` on the multi-threaded runtime.

// The cases are tests: they panic on anything unexpected, by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod cases;
pub mod fixtures;
pub mod payments;
pub mod price_cases;
pub mod retention;
pub mod top_up;

/// Runs every conformance case against the store `$factory` makes: `async fn() -> Option<S>`, a fresh empty store
/// per case, or `None` to skip it.
#[macro_export]
macro_rules! ledger_conformance {
    ($factory:path) => {
        $crate::__ledger_cases!(
            $factory;
            the_same_batch_twice_debits_once,
            a_bad_event_refuses_the_whole_batch_and_writes_nothing,
            concurrent_intake_and_drains_sum_exactly,
            concurrent_drains_with_scarce_credit_split_credit_and_debt_exactly,
            concurrent_drains_over_many_owners_neither_deadlock_nor_lose_money,
            a_payment_credits_once,
            a_payment_id_credits_once_by_either_route,
            sources_reversals_and_debt_repayment,
            reversals_are_capped_at_the_payment,
            concurrent_reversals_never_pass_the_cap,
            a_credit_replay_must_match_its_terms,
            the_debit_draws_in_the_configured_order,
            the_draw_order_is_configuration,
            allowed_refuses_at_zero_or_below,
            the_member_cap_is_per_owner_currency_member_and_month,
            an_event_outside_the_window_refuses_the_batch_naming_it,
            sums_that_overflow_are_refused_and_a_debt_never_overflows,
            a_repeat_counts_once_and_a_differing_payload_is_refused,
            the_same_id_on_another_day_is_refused_as_id_reused,
            an_owner_without_an_account_in_the_currency_is_refused,
            a_debit_past_the_debt_limit_is_booked_as_debt_and_flags_the_owner,
            allowed_compares_as_the_policy_says,
            the_allowance_is_granted_once_per_account_and_month,
            adjustments_add_and_take_credit,
            accounts_are_once_only_and_one_per_owner_currency_and_label,
            daily_usage_sums_applied_debits_per_utc_day,
            invalid_amounts_and_configurations_are_refused,
        );
    };
}

/// Runs every conformance case of partition retention (ADR 0001, point 10) against the store `$factory` makes:
/// `async fn() -> Option<S>` where `S` implements [`RetentionFixture`](crate::retention::RetentionFixture), a fresh
/// store per case with its partitions made, or `None` to skip it.
#[macro_export]
macro_rules! retention_conformance {
    ($factory:path) => {
        $crate::__retention_cases!(
            $factory;
            an_event_replayed_inside_the_window_is_still_refused_after_a_drop,
            a_partition_with_an_undecided_event_is_kept,
            a_partition_inside_the_window_plus_the_margin_is_kept,
            the_window_and_the_current_partition_are_never_touched,
            concurrent_runs_drop_each_partition_once,
        );
    };
}

/// Runs every conformance case of top-ups (ADR 0003, amended 2026-11-28) against the store `$factory` makes:
/// `async fn(UnrecordedTopUps) -> Option<S>` where `S` implements [`LedgerStore`](openbilling_core::LedgerStore) and
/// [`TopUpLedger`](openbilling_core::TopUpLedger), a fresh empty store per call that applies the given policy for a
/// payment no top-up was recorded for, or `None` to skip it.
#[macro_export]
macro_rules! top_up_conformance {
    ($factory:path) => {
        $crate::__top_up_cases!(
            $factory;
            a_top_up_is_recorded_once_and_other_terms_conflict,
            a_payment_that_differs_credits_nothing_and_stays_refused,
            an_unrecorded_payment_follows_the_policy,
            a_succeeded_payment_credits_the_recorded_terms_once,
            a_canceled_top_up_is_final,
            claims_respect_the_windows_and_are_disjoint,
        );
    };
}

/// Runs every conformance case of the price book (ADR 0004) against the store `$factory` makes: `async fn() ->
/// Option<S>` where `S` implements [`PriceBook`](openbilling_core::PriceBook), a fresh empty store per case, or
/// `None` to skip it.
#[macro_export]
macro_rules! price_book_conformance {
    ($factory:path) => {
        $crate::__price_book_cases!(
            $factory;
            the_same_event_rated_twice_gives_the_same_amount_and_version,
            a_new_version_rates_only_later_events,
            an_unpriced_meter_or_dimension_is_refused,
            rounding_is_half_to_even_once_per_event,
            a_huge_quantity_is_exact_or_refused,
            a_malformed_price_list_names_its_error,
            loading_the_same_list_twice_is_a_no_op,
            a_version_is_never_back_dated,
            the_payload_of_a_raw_event_is_its_meter_quantity_and_dimensions,
        );
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __price_book_cases {
    ($factory:path; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                if let Some(store) = $factory().await {
                    $crate::price_cases::$case(store).await;
                }
            }
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __ledger_cases {
    ($factory:path; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                if let Some(store) = $factory().await {
                    $crate::cases::$case(store).await;
                }
            }
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __retention_cases {
    ($factory:path; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                if let Some(store) = $factory().await {
                    $crate::retention::$case(store).await;
                }
            }
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __top_up_cases {
    ($factory:path; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                $crate::top_up::$case($factory).await;
            }
        )*
    };
}
