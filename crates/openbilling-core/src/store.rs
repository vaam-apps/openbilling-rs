//! The storage port of the ledger, [`LedgerStore`], and what goes in and out of it.
//!
//! The port is a trait whose methods return futures; this crate has no runtime and does no I/O. The PostgreSQL
//! store implements it in `openbilling-pg`; `openbilling-testkit` holds the conformance suite every implementation
//! runs, and an in-memory implementation that runs it without a database.

use std::future::Future;

use crate::amount::Amount;
use crate::config::{CreditSource, LedgerConfig, LotKind, ReversalKind};
use crate::id::{AccountId, AccountLabel, CreditId, CurrencyCode, EventId, MemberId, OwnerId};
use crate::outcome::EventOutcome;
use crate::price::PriceVersionId;
use crate::refusal::LedgerError;
use crate::time::{Timestamp, UtcDay, UtcMonth};
use crate::usage::{IntakeReport, UsageEvent};

/// An account to open: one currency and one label of one owner (ADR 0002).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NewAccount {
    /// Its id, given by whoever opens it.
    pub id: AccountId,
    /// Its owner.
    pub owner: OwnerId,
    /// Its one currency.
    pub currency: CurrencyCode,
    /// Its label, one the operator has configured.
    pub label: AccountLabel,
}

/// The answer to opening an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Opened {
    /// The same account was already open; nothing changed.
    pub replayed: bool,
}

/// Which account a credit goes to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CreditTarget {
    /// By its id.
    Account(AccountId),
    /// By its owner, currency and label.
    Label {
        /// The owner.
        owner: OwnerId,
        /// The currency.
        currency: CurrencyCode,
        /// The label.
        label: AccountLabel,
    },
}

/// A credit: money paid, promotional credit, or an operator's correction (ADR 0002).
///
/// Once-only by `(source, id)`. The same id again with the same account, amount and expiry is a replay; with
/// anything else it is a conflict, never a second credit. The store refuses a payment or a promotion that is not
/// positive, an adjustment of zero, and an expiry on a negative adjustment (`invalid_amount`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Credit {
    /// The source.
    pub source: CreditSource,
    /// The caller's id, once-only within the source.
    pub id: CreditId,
    /// The account it goes to.
    pub to: CreditTarget,
    /// Positive opens a lot; negative (an adjustment only) takes credit away from the account's lots.
    pub amount: Amount,
    /// When the lot it opens expires, if it does.
    pub expires_at: Option<Timestamp>,
}

/// A refund or a chargeback of a payment. Once-only by `(kind, id)`; refunds and chargebacks of one payment
/// together never take back more than the payment (`reversal_cap`). The store refuses an amount that is not
/// positive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reversal {
    /// Refund or chargeback.
    pub kind: ReversalKind,
    /// Its own id, once-only within its kind.
    pub id: CreditId,
    /// The payment it reverses.
    pub payment: CreditId,
    /// What it takes back: positive.
    pub amount: Amount,
}

/// The answer to a credit, a reversal or an allowance.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Credited {
    /// The account it was booked on.
    pub account: AccountId,
    /// The ledger transaction that booked it, as text. A replay answers the first one's.
    pub transaction: String,
    /// It was already booked; nothing changed.
    pub replayed: bool,
    /// How much of the owner's debt in that currency it repaid. Zero on a replay.
    pub repaid: Amount,
    /// How much it added to the owner's debt: what a negative credit could not take from lots. Zero on a replay.
    pub debt_added: Amount,
}

/// One decision of a drain.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decided {
    /// The event.
    pub event: EventId,
    /// Its occurrence day: with the id, the event's key.
    pub occurred_on: UtcDay,
    /// Applied, or refused and why.
    pub outcome: EventOutcome,
    /// Its amount.
    pub amount: Amount,
    /// The price book version that rated it, for a raw event; `None` for a priced one (ADR 0004).
    pub price_version: Option<PriceVersionId>,
}

/// What one drain did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// One decision per event taken, in the order they were taken. Empty when there was nothing to take.
    pub decided: Vec<Decided>,
}

impl DrainReport {
    /// How many events were applied.
    #[must_use]
    pub fn applied(&self) -> usize {
        self.decided
            .iter()
            .filter(|d| d.outcome == EventOutcome::Applied)
            .count()
    }

    /// The sum of the applied events' amounts, saturating.
    #[must_use]
    pub fn debited(&self) -> Amount {
        self.decided
            .iter()
            .filter(|d| d.outcome == EventOutcome::Applied)
            .fold(Amount::ZERO, |sum, d| {
                sum.checked_add(d.amount).unwrap_or(Amount::MAX)
            })
    }
}

/// One account's spendable credit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountBalance {
    /// The account.
    pub account: AccountId,
    /// Its label.
    pub label: AccountLabel,
    /// What its drawable lots hold, saturating.
    pub available: Amount,
}

/// An owner's balance in one currency, at one instant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Balance {
    /// Each account, in draw order.
    pub accounts: Vec<AccountBalance>,
    /// What the drawable lots of every account hold, saturating.
    pub available: Amount,
    /// What found no credit. Never negative; new credit repays it first.
    pub debt: Amount,
}

impl Balance {
    /// `available − debt`. Negative when the owner overshot.
    #[must_use]
    pub fn net(&self) -> Amount {
        self.available.checked_sub(self.debt).unwrap_or(Amount::MIN)
    }
}

/// Why [`Allowed`] says no.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotAllowed {
    /// The owner's debt in the currency reached its debt limit, as the policy compares them.
    DebtLimit,
    /// The owner's debt in the currency reached what its drawable lots hold, as the policy compares them.
    Balance,
    /// The member's applied usage this month reached their cap, as the policy compares them.
    MemberCap,
}

impl NotAllowed {
    /// The reason's stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DebtLimit => "debt_limit",
            Self::Balance => "balance",
            Self::MemberCap => "member_cap",
        }
    }
}

/// When one check of [`LedgerStore::allowed`] refuses: a plain comparison of what is used against its bound, with
/// no special case (the owner's decision of 2026-11-27: "let the end developer decide how in the .yaml
/// configuration"). The deployer chooses it; the store only compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RefuseWhen {
    /// Refuse when what is used is at or above the bound (`used >= bound`). The default.
    #[default]
    AtOrAbove,
    /// Refuse when what is used is above the bound (`used > bound`).
    Above,
    /// Never refuse: report the numbers only.
    Never,
}

impl RefuseWhen {
    /// Every comparison, in a stable order.
    pub const ALL: [Self; 3] = [Self::AtOrAbove, Self::Above, Self::Never];

    /// The comparison's stable name, as the configuration file and the database name it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AtOrAbove => "at_or_above",
            Self::Above => "above",
            Self::Never => "never",
        }
    }

    /// The comparison of a stable name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

/// How [`LedgerStore::allowed`] turns its numbers into a verdict: one [`RefuseWhen`] per check, compared in this
/// order. The default is [`RefuseWhen::AtOrAbove`] for each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AllowedPolicy {
    /// The owner's debt against its debt limit. With no limit, this check never refuses.
    pub debt_limit: RefuseWhen,
    /// The owner's debt against what its drawable lots hold: `at_or_above` refuses a net balance of zero or below,
    /// `above` one below zero.
    pub balance: RefuseWhen,
    /// The member's applied usage in the UTC month against their cap. With no cap, this check never refuses.
    pub member_cap: RefuseWhen,
}

/// May the member spend for the owner in this currency now? A read for whoever serves the usage; the ledger itself
/// never refuses a debit for it. The store compares the numbers as the [`AllowedPolicy`] says, in its order: the
/// debt limit, then the balance, then the member's cap.
///
/// The numbers are facts, whatever the policy: a caller with a rule of its own reads them and ignores `refused`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Allowed {
    /// Why not, or `None` when allowed.
    pub refused: Option<NotAllowed>,
    /// `available − debt`, signed: negative says the owner overshot.
    pub balance: Amount,
    /// What the owner's drawable lots in the currency hold, saturating.
    pub available: Amount,
    /// What the owner owes in the currency.
    pub debt: Amount,
    /// The owner's debt limit in the currency, if it has one.
    pub debt_limit: Option<Amount>,
    /// Whether the debt is above the limit (`debt > limit`): the flag of the reads, whatever the policy.
    pub past_limit: bool,
    /// The member's applied usage in the UTC month, saturating.
    pub member_spent: Amount,
    /// The member's cap this month, if they have one.
    pub member_cap: Option<Amount>,
    /// `cap − spent`, when the member has a cap.
    pub member_remaining: Option<Amount>,
}

impl Allowed {
    /// Whether the member may spend.
    #[must_use]
    pub const fn allowed(&self) -> bool {
        self.refused.is_none()
    }
}

/// A lot, as the store shows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LotView {
    /// Its kind.
    pub kind: LotKind,
    /// The credit that opened it, or `None` for an allowance.
    pub credit: Option<CreditId>,
    /// The month, for an allowance.
    pub month: Option<UtcMonth>,
    /// What opened it.
    pub amount: Amount,
    /// What is left.
    pub remaining: Amount,
    /// When it expires, if it does.
    pub expires_at: Option<Timestamp>,
}

/// An event's durable outcome (ADR 0001, point 14).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutcomeView {
    /// Its occurrence day.
    pub occurred_on: UtcDay,
    /// Applied, or refused and why.
    pub outcome: EventOutcome,
    /// Its amount.
    pub amount: Amount,
    /// The price book version that rated it, for a raw event; `None` for a priced one (ADR 0004).
    pub price_version: Option<PriceVersionId>,
}

/// The ledger's storage: accounts, credits, the usage queue and its drain, and the reads over them.
///
/// Every write is once-only by a caller-given id (agent guide, rule 2), and every refusal is a
/// [`LedgerError::Refused`] that wrote nothing.
pub trait LedgerStore: Send + Sync {
    /// The store's clock: the one the window is checked against.
    fn now(&self) -> impl Future<Output = Result<Timestamp, LedgerError>> + Send;

    /// Sets the configuration: adds the currencies and labels it names, orders the labels and sets the lot order.
    fn configure(
        &self,
        config: &LedgerConfig,
    ) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// Opens an account.
    fn open_account(
        &self,
        account: &NewAccount,
    ) -> impl Future<Output = Result<Opened, LedgerError>> + Send;

    /// Sets, or with `None` removes, how much debt an owner may run up in a currency. With none, debt is unlimited.
    /// The limit never refuses a debit: usage that already happened is booked as debt even past it (the owner's
    /// decision of 2026-11-27, "Always book as debt"). It refuses new calls through [`LedgerStore::allowed`]
    /// ([`NotAllowed::DebtLimit`]), as the deployer's [`AllowedPolicy`] compares it.
    fn set_debt_limit(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        limit: Option<Amount>,
    ) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// Sets, or with `None` removes, a member's monthly cap for an owner in a currency.
    fn set_member_cap(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        member: &MemberId,
        cap: Option<Amount>,
    ) -> impl Future<Output = Result<(), LedgerError>> + Send;

    /// Grants an account its allowance for `month`, once: a lot spendable during that month only.
    fn grant_allowance(
        &self,
        account: &AccountId,
        month: UtcMonth,
        amount: Amount,
    ) -> impl Future<Output = Result<Credited, LedgerError>> + Send;

    /// Books a credit, once. New credit repays the owner's debt in its currency first.
    fn credit(&self, credit: &Credit)
    -> impl Future<Output = Result<Credited, LedgerError>> + Send;

    /// Books a refund or a chargeback of a payment, once. What the payment's lot no longer holds becomes debt.
    fn reverse(
        &self,
        reversal: &Reversal,
    ) -> impl Future<Output = Result<Credited, LedgerError>> + Send;

    /// Takes a batch of priced usage events into the queue, all or nothing (ADR 0001, points 1 to 5).
    fn intake(
        &self,
        events: &[UsageEvent],
    ) -> impl Future<Output = Result<IntakeReport, LedgerError>> + Send;

    /// Applies up to `limit` received events in one transaction (ADR 0001, points 11 to 15). Rows another drain
    /// holds are skipped, never waited for.
    fn drain(&self, limit: u32) -> impl Future<Output = Result<DrainReport, LedgerError>> + Send;

    /// An owner's balance in a currency at `at`.
    fn balance(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        at: Timestamp,
    ) -> impl Future<Output = Result<Balance, LedgerError>> + Send;

    /// Whether `member` may spend for `owner` in `currency` at `at`: the balance at `at` and the member's spend in
    /// the UTC month of `at`, compared as `policy` says. The numbers it answers do not depend on `policy`.
    fn allowed(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        member: &MemberId,
        at: Timestamp,
        policy: AllowedPolicy,
    ) -> impl Future<Output = Result<Allowed, LedgerError>> + Send;

    /// An account's lots, in draw order, drawable or not.
    fn lots(
        &self,
        account: &AccountId,
    ) -> impl Future<Output = Result<Vec<LotView>, LedgerError>> + Send;

    /// An event's outcome, once a drain decided it. `None` before, and for an event refused as `id_reused`, whose
    /// id's outcome belongs to the event that came first.
    fn outcome(
        &self,
        event: &EventId,
    ) -> impl Future<Output = Result<Option<OutcomeView>, LedgerError>> + Send;

    /// The applied usage of an owner in a currency, summed per UTC day of occurrence, oldest first.
    fn daily_usage(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
    ) -> impl Future<Output = Result<Vec<(UtcDay, Amount)>, LedgerError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(micros: i64) -> Amount {
        Amount::from_micros(micros).expect("amount")
    }

    #[test]
    fn allowed_is_allowed_when_nothing_refused_it() {
        let mut answer = Allowed {
            refused: None,
            balance: amount(1),
            available: amount(1),
            debt: Amount::ZERO,
            debt_limit: None,
            past_limit: false,
            member_spent: Amount::ZERO,
            member_cap: None,
            member_remaining: None,
        };
        assert!(answer.allowed());
        answer.refused = Some(NotAllowed::MemberCap);
        assert!(!answer.allowed());
        assert_eq!(NotAllowed::DebtLimit.as_str(), "debt_limit");
        assert_eq!(NotAllowed::Balance.as_str(), "balance");
        assert_eq!(NotAllowed::MemberCap.as_str(), "member_cap");
    }

    #[test]
    fn a_comparison_has_a_stable_name_and_the_default_is_at_or_above() {
        let names: Vec<&str> = RefuseWhen::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(names, ["at_or_above", "above", "never"]);
        for mode in RefuseWhen::ALL {
            assert_eq!(RefuseWhen::parse(mode.as_str()), Some(mode));
        }
        for bad in ["", "At_Or_Above", "at-or-above", "below", " above"] {
            assert_eq!(RefuseWhen::parse(bad), None, "{bad:?}");
        }
        let policy = AllowedPolicy::default();
        assert_eq!(
            [policy.debt_limit, policy.balance, policy.member_cap],
            [RefuseWhen::AtOrAbove; 3]
        );
    }

    #[test]
    fn a_balance_nets_its_debt() {
        let balance = Balance {
            accounts: Vec::new(),
            available: amount(100),
            debt: amount(140),
        };
        assert_eq!(balance.net(), amount(-40));
    }

    #[test]
    fn a_drain_report_counts_what_it_applied() {
        let day = UtcDay::from_unix_days(1).expect("day");
        let decided = |n: &str, outcome, micros| Decided {
            event: EventId::new(n).expect("id"),
            occurred_on: day,
            outcome,
            amount: amount(micros),
            price_version: None,
        };
        let report = DrainReport {
            decided: vec![
                decided("a", EventOutcome::Applied, 5),
                decided(
                    "b",
                    EventOutcome::Refused(crate::RefusalReason::Overflow),
                    7,
                ),
                decided("c", EventOutcome::Applied, 1),
            ],
        };
        assert_eq!(report.applied(), 2);
        assert_eq!(report.debited(), amount(6));
    }
}
