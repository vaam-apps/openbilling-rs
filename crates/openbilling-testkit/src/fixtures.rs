//! Helpers the conformance cases share. They panic on anything unexpected: they are test code.

use openbilling_core::{
    AccountId, AccountLabel, Allowed, Amount, CreditId, CreditSource, CreditTarget, Credited,
    CurrencyCode, DrainReport, EventId, LedgerConfig, LedgerError, LedgerStore, LotOrder, LotView,
    MemberId, NewAccount, OwnerId, Rule, Timestamp, UsageEvent,
};

/// The euro: the currency most cases use.
pub fn eur() -> CurrencyCode {
    CurrencyCode::new("EUR").expect("a currency")
}

/// A second configured currency.
pub fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("a currency")
}

/// An owner.
pub fn owner(id: &str) -> OwnerId {
    OwnerId::new(id).expect("an owner id")
}

/// An account id.
pub fn account(id: &str) -> AccountId {
    AccountId::new(id).expect("an account id")
}

/// A member.
pub fn member(id: &str) -> MemberId {
    MemberId::new(id).expect("a member id")
}

/// A credit id.
pub fn credit_id(id: &str) -> CreditId {
    CreditId::new(id).expect("a credit id")
}

/// An event id.
pub fn event_id(id: &str) -> EventId {
    EventId::new(id).expect("an event id")
}

/// A label.
pub fn label(name: &str) -> AccountLabel {
    AccountLabel::new(name).expect("a label")
}

/// `micros` micro-units.
pub fn micros(micros: i64) -> Amount {
    Amount::from_micros(micros).expect("an amount")
}

/// `at` moved by `micros` microseconds.
pub fn plus(at: Timestamp, micros: i64) -> Timestamp {
    at.checked_add_micros(micros).expect("in range")
}

/// Microseconds in a day.
pub const DAY: i64 = 86_400_000_000;
/// Microseconds in an hour.
pub const HOUR: i64 = 3_600_000_000;
/// Microseconds in a minute.
pub const MINUTE: i64 = 60_000_000;

/// The configuration every case starts from: EUR and USD; labels `promo` then `main`; the default lot order.
pub fn config() -> LedgerConfig {
    LedgerConfig {
        currencies: vec![eur(), usd()],
        labels: vec![label("promo"), label("main")],
        lot_order: LotOrder::default(),
    }
}

/// Applies [`config`].
pub async fn setup<S: LedgerStore>(store: &S) {
    store.configure(&config()).await.expect("configure");
}

/// The store's clock.
pub async fn now<S: LedgerStore>(store: &S) -> Timestamp {
    store.now().await.expect("now")
}

/// Opens account `id` for `owner` in `currency` with `label`.
pub async fn open<S: LedgerStore>(
    store: &S,
    id: &str,
    who: &str,
    currency: CurrencyCode,
    name: &str,
) {
    store
        .open_account(&NewAccount {
            id: account(id),
            owner: owner(who),
            currency,
            label: label(name),
        })
        .await
        .expect("open an account");
}

/// A credit of `source` to the account `to`.
pub fn credit_to(
    source: CreditSource,
    id: &str,
    to: &str,
    amount: i64,
    expires: Option<Timestamp>,
) -> openbilling_core::Credit {
    openbilling_core::Credit {
        source,
        id: credit_id(id),
        to: CreditTarget::Account(account(to)),
        amount: micros(amount),
        expires_at: expires,
    }
}

/// Books a payment, which must succeed.
pub async fn pay<S: LedgerStore>(store: &S, payment: &str, to: &str, amount: i64) -> Credited {
    store
        .credit(&credit_to(CreditSource::Payment, payment, to, amount, None))
        .await
        .expect("a payment")
}

/// A EUR event of `who` by `by`.
pub fn event(id: &str, who: &str, by: &str, amount: i64, at: Timestamp) -> UsageEvent {
    UsageEvent {
        id: event_id(id),
        owner: owner(who),
        currency: eur(),
        member: member(by),
        amount: micros(amount),
        occurred_at: at,
    }
}

/// Takes `events` in, which must succeed.
pub async fn take<S: LedgerStore>(
    store: &S,
    events: &[UsageEvent],
) -> openbilling_core::IntakeReport {
    store.intake(events).await.expect("intake")
}

/// Drains until a drain takes nothing; returns every decision.
pub async fn drain_all<S: LedgerStore>(store: &S, limit: u32) -> DrainReport {
    let mut all = DrainReport::default();
    loop {
        let report = store.drain(limit).await.expect("drain");
        if report.decided.is_empty() {
            return all;
        }
        all.decided.extend(report.decided);
    }
}

/// An account's lots.
pub async fn lots<S: LedgerStore>(store: &S, id: &str) -> Vec<LotView> {
    store.lots(&account(id)).await.expect("lots")
}

/// What each of an account's lots holds, in draw order.
pub async fn remainings<S: LedgerStore>(store: &S, id: &str) -> Vec<i64> {
    lots(store, id)
        .await
        .iter()
        .map(|lot| lot.remaining.micros())
        .collect()
}

/// Whether `by` may spend for `who` in EUR at `at`.
pub async fn allowed_at<S: LedgerStore>(store: &S, who: &str, by: &str, at: Timestamp) -> Allowed {
    store
        .allowed(&owner(who), &eur(), &member(by), at)
        .await
        .expect("allowed")
}

/// Whether `by` may spend for `who` in EUR now.
pub async fn allowed_now<S: LedgerStore>(store: &S, who: &str, by: &str) -> Allowed {
    let at = now(store).await;
    allowed_at(store, who, by, at).await
}

/// What an account's lots hold, drawable or not.
pub async fn remaining<S: LedgerStore>(store: &S, id: &str) -> i64 {
    lots(store, id)
        .await
        .iter()
        .map(|lot| lot.remaining.micros())
        .sum()
}

/// An owner's debt in EUR.
pub async fn debt<S: LedgerStore>(store: &S, who: &str) -> i64 {
    let at = now(store).await;
    store
        .balance(&owner(who), &eur(), at)
        .await
        .expect("balance")
        .debt
        .micros()
}

/// An owner's net EUR balance now.
pub async fn net<S: LedgerStore>(store: &S, who: &str) -> i64 {
    let at = now(store).await;
    store
        .balance(&owner(who), &eur(), at)
        .await
        .expect("balance")
        .net()
        .micros()
}

/// The rule of a refusal, failing on anything else.
pub fn rule_of<T: std::fmt::Debug>(result: Result<T, LedgerError>) -> Rule {
    match result {
        Err(LedgerError::Refused(refusal)) => refusal.rule(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The rule and the named events of a refusal.
pub fn refusal_of<T: std::fmt::Debug>(result: Result<T, LedgerError>) -> (Rule, Vec<String>) {
    match result {
        Err(LedgerError::Refused(refusal)) => (
            refusal.rule(),
            refusal
                .events()
                .iter()
                .map(|e| e.as_str().to_owned())
                .collect(),
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
