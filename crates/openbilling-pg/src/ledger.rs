//! [`PgLedger`]: the [`LedgerStore`] port on PostgreSQL.
//!
//! A thin caller (agent guide, rule 12): each method is one statement that calls one function of the migrations, in
//! its own transaction, and maps the answer to the port's types. No rule of the ledger is written here.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use openbilling_core::{
    AccountBalance, AccountId, AccountLabel, Allowed, Amount, Balance, CreditId, CreditTarget,
    Credited, CurrencyCode, Decided, DrainReport, EventId, EventOutcome, Intake, IntakeReport,
    LedgerConfig, LedgerError, LedgerStore, LotKind, LotView, MemberId, NewAccount, NotAllowed,
    Opened, OutcomeView, OwnerId, RefusalReason, Reversal, Timestamp, UsageEvent, UtcDay, UtcMonth,
    Window, WithinKind,
};
use sqlx::PgPool;
use sqlx::postgres::PgListener;

use crate::sqlstate::{corrupt, ledger_error};
use crate::{Error, PgStore};

/// The channel intake notifies and workers listen on. A notification carries no data (ADR 0001, point 16).
pub const NOTIFY_CHANNEL: &str = "ob_usage";

/// How the store runs: the window intake checks events against, the partitions it makes ahead, and how often intake
/// may notify the workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerSettings {
    /// The intake window (ADR 0001, point 8).
    pub window: Window,
    /// How many days ahead [`PgLedger::prepare_partitions`] makes partitions (default 2).
    pub partitions_ahead_days: u16,
    /// At most one `NOTIFY` per this interval per process (ADR 0001, point 17).
    pub notify_interval: Duration,
}

impl Default for LedgerSettings {
    fn default() -> Self {
        Self {
            window: Window::default(),
            partitions_ahead_days: 2,
            notify_interval: Duration::from_millis(100),
        }
    }
}

/// The ledger on PostgreSQL. Clones share one pool.
#[derive(Clone, Debug)]
pub struct PgLedger {
    pool: PgPool,
    settings: LedgerSettings,
    last_notify: Arc<Mutex<Option<Instant>>>,
}

impl PgLedger {
    /// The ledger on `store`'s pool. The schema must be migrated ([`PgStore::migrate`]).
    #[must_use]
    pub fn new(store: &PgStore, settings: LedgerSettings) -> Self {
        Self {
            pool: store.pool().clone(),
            settings,
            last_notify: Arc::default(),
        }
    }

    /// The settings.
    #[must_use]
    pub const fn settings(&self) -> LedgerSettings {
        self.settings
    }

    /// Makes the partitions of every day intake accepts now, and of the days ahead (ADR 0001, point 10). Returns how
    /// many it made. Run it at start and then regularly: an event for a day with no partition is unavailable.
    ///
    /// # Errors
    ///
    /// A [`LedgerError::Store`] when the database fails.
    pub async fn prepare_partitions(&self) -> Result<u32, LedgerError> {
        let made: i32 = sqlx::query_scalar("SELECT ob_prepare_partitions($1, $2)")
            .bind(i32::from(self.settings.window.days()))
            .bind(i32::from(self.settings.partitions_ahead_days))
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        u32::try_from(made).map_err(|_| corrupt())
    }

    /// A listener for the workers' wake-up. It holds a connection of its own, which must be a direct session: `LISTEN`
    /// does not work behind a pooler in transaction mode.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when the connection or the `LISTEN` fails.
    pub async fn wakeup(&self) -> Result<Wakeup, Error> {
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(Error::Connect)?;
        listener
            .listen(NOTIFY_CHANNEL)
            .await
            .map_err(Error::Query)?;
        Ok(Wakeup { listener })
    }

    /// Notifies the workers, at most once per [`LedgerSettings::notify_interval`], in its own transaction after
    /// intake committed. A notification that fails or is skipped costs at most one poll: the table is the truth.
    async fn notify(&self) {
        let due = {
            let mut last = self
                .last_notify
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let due = last.is_none_or(|at| at.elapsed() >= self.settings.notify_interval);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if due {
            // Ignored on purpose: intake has committed, and a lost wake-up is covered by the poll.
            let _ = sqlx::query("SELECT pg_notify($1, '')")
                .bind(NOTIFY_CHANNEL)
                .execute(&self.pool)
                .await;
        }
    }
}

/// Why [`Wakeup::wait`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woken {
    /// A notification came.
    Notified,
    /// The poll interval passed with none.
    Polled,
}

/// A worker's wake-up: a notification on [`NOTIFY_CHANNEL`], or the poll interval.
#[derive(Debug)]
pub struct Wakeup {
    listener: PgListener,
}

impl Wakeup {
    /// Waits for a notification, or `poll` at most.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when the listening connection fails.
    pub async fn wait(&mut self, poll: Duration) -> Result<Woken, Error> {
        match tokio::time::timeout(poll, self.listener.recv()).await {
            Ok(Ok(_)) => Ok(Woken::Notified),
            Ok(Err(error)) => Err(Error::Query(error)),
            Err(_) => Ok(Woken::Polled),
        }
    }
}

/// A lot as `ob_lots_of` returns it: kind, credit id, allowance month (days since the epoch), amount, remaining,
/// expiry (microseconds).
type LotRow = (String, Option<String>, Option<i32>, i64, i64, Option<i64>);

fn amount(micros: i64) -> Result<Amount, LedgerError> {
    Amount::from_micros(micros).map_err(|_| corrupt())
}

fn day(days: i32) -> Result<UtcDay, LedgerError> {
    UtcDay::from_unix_days(i64::from(days)).map_err(|_| corrupt())
}

fn credited(row: (String, String, bool, i64, i64)) -> Result<Credited, LedgerError> {
    let (account, transaction, replayed, repaid, debt_added) = row;
    Ok(Credited {
        account: AccountId::new(account).map_err(|_| corrupt())?,
        transaction,
        replayed,
        repaid: amount(repaid)?,
        debt_added: amount(debt_added)?,
    })
}

fn outcome(state: &str, reason: Option<&str>) -> Result<EventOutcome, LedgerError> {
    match (state, reason) {
        ("applied", None) => Ok(EventOutcome::Applied),
        ("refused", Some(reason)) => RefusalReason::parse(reason)
            .map(EventOutcome::Refused)
            .ok_or_else(corrupt),
        _ => Err(corrupt()),
    }
}

impl LedgerStore for PgLedger {
    async fn now(&self) -> Result<Timestamp, LedgerError> {
        let micros: i64 = sqlx::query_scalar("SELECT ob_micros(now())")
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        Timestamp::from_unix_micros(micros).map_err(|_| corrupt())
    }

    async fn configure(&self, config: &LedgerConfig) -> Result<(), LedgerError> {
        let currencies: Vec<&str> = config.currencies.iter().map(CurrencyCode::as_str).collect();
        let labels: Vec<&str> = config.labels.iter().map(AccountLabel::as_str).collect();
        let kinds: Vec<&str> = config.lot_order.0.iter().map(|(k, _)| k.as_str()).collect();
        let soonest: Vec<bool> = config
            .lot_order
            .0
            .iter()
            .map(|(_, within)| *within == WithinKind::SoonestExpiryFirst)
            .collect();
        sqlx::query("SELECT ob_configure($1, $2, $3, $4)")
            .bind(currencies)
            .bind(labels)
            .bind(kinds)
            .bind(soonest)
            .execute(&self.pool)
            .await
            .map_err(ledger_error)?;
        Ok(())
    }

    async fn open_account(&self, account: &NewAccount) -> Result<Opened, LedgerError> {
        let replayed: bool = sqlx::query_scalar("SELECT ob_open_account($1, $2, $3, $4)")
            .bind(account.id.as_str())
            .bind(account.owner.as_str())
            .bind(account.currency.as_str())
            .bind(account.label.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        Ok(Opened { replayed })
    }

    async fn set_debt_limit(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        limit: Option<Amount>,
    ) -> Result<(), LedgerError> {
        sqlx::query("SELECT ob_set_debt_limit($1, $2, $3)")
            .bind(owner.as_str())
            .bind(currency.as_str())
            .bind(limit.map(Amount::micros))
            .execute(&self.pool)
            .await
            .map_err(ledger_error)?;
        Ok(())
    }

    async fn set_member_cap(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        member: &MemberId,
        cap: Option<Amount>,
    ) -> Result<(), LedgerError> {
        sqlx::query("SELECT ob_set_member_cap($1, $2, $3, $4)")
            .bind(owner.as_str())
            .bind(currency.as_str())
            .bind(member.as_str())
            .bind(cap.map(Amount::micros))
            .execute(&self.pool)
            .await
            .map_err(ledger_error)?;
        Ok(())
    }

    async fn grant_allowance(
        &self,
        account: &AccountId,
        month: UtcMonth,
        amount: Amount,
    ) -> Result<Credited, LedgerError> {
        let row = sqlx::query_as(
            "SELECT o_account_id, o_txn_id::text, o_replayed, o_repaid, o_debt_added \
             FROM ob_grant_allowance($1, $2, $3, $4)",
        )
        .bind(account.as_str())
        .bind(month.year())
        .bind(i32::from(month.month()))
        .bind(amount.micros())
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        credited(row)
    }

    async fn credit(&self, credit: &openbilling_core::Credit) -> Result<Credited, LedgerError> {
        let (account, owner, currency, label) = match &credit.to {
            CreditTarget::Account(id) => (Some(id.as_str()), None, None, None),
            CreditTarget::Label {
                owner,
                currency,
                label,
            } => (
                None,
                Some(owner.as_str()),
                Some(currency.as_str()),
                Some(label.as_str()),
            ),
        };
        let row = sqlx::query_as(
            "SELECT o_account_id, o_txn_id::text, o_replayed, o_repaid, o_debt_added \
             FROM ob_credit($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(credit.source.as_str())
        .bind(credit.id.as_str())
        .bind(account)
        .bind(owner)
        .bind(currency)
        .bind(label)
        .bind(credit.amount.micros())
        .bind(credit.expires_at.map(Timestamp::unix_micros))
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        credited(row)
    }

    async fn reverse(&self, reversal: &Reversal) -> Result<Credited, LedgerError> {
        let row = sqlx::query_as(
            "SELECT o_account_id, o_txn_id::text, o_replayed, o_repaid, o_debt_added \
             FROM ob_reverse($1, $2, $3, $4)",
        )
        .bind(reversal.kind.as_str())
        .bind(reversal.id.as_str())
        .bind(reversal.payment.as_str())
        .bind(reversal.amount.micros())
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        credited(row)
    }

    async fn intake(&self, events: &[UsageEvent]) -> Result<IntakeReport, LedgerError> {
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        let owners: Vec<&str> = events.iter().map(|e| e.owner.as_str()).collect();
        let currencies: Vec<&str> = events.iter().map(|e| e.currency.as_str()).collect();
        let members: Vec<&str> = events.iter().map(|e| e.member.as_str()).collect();
        let amounts: Vec<i64> = events.iter().map(|e| e.amount.micros()).collect();
        let occurred: Vec<i64> = events.iter().map(|e| e.occurred_at.unix_micros()).collect();
        let window = self.settings.window;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT o_event_id, o_answer FROM ob_intake($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(ids)
        .bind(owners)
        .bind(currencies)
        .bind(members)
        .bind(amounts)
        .bind(occurred)
        .bind(i32::from(window.days()))
        .bind(window.skew_micros())
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        let answers = rows
            .into_iter()
            .map(|(id, answer)| {
                let id = EventId::new(id).map_err(|_| corrupt())?;
                let answer = match answer.as_str() {
                    "accepted" => Intake::Accepted,
                    "replayed" => Intake::Replayed,
                    _ => return Err(corrupt()),
                };
                Ok((id, answer))
            })
            .collect::<Result<Vec<_>, LedgerError>>()?;
        let report = IntakeReport { answers };
        if report.accepted() > 0 {
            self.notify().await;
        }
        Ok(report)
    }

    async fn drain(&self, limit: u32) -> Result<DrainReport, LedgerError> {
        let rows: Vec<(String, i32, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT o_event_id, o_occurred_day, o_state, o_reason, o_amount FROM ob_drain($1)",
        )
        .bind(i32::try_from(limit).unwrap_or(i32::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        let decided = rows
            .into_iter()
            .map(|(id, occurred_on, state, reason, micros)| {
                Ok(Decided {
                    event: EventId::new(id).map_err(|_| corrupt())?,
                    occurred_on: day(occurred_on)?,
                    outcome: outcome(&state, reason.as_deref())?,
                    amount: amount(micros)?,
                })
            })
            .collect::<Result<Vec<_>, LedgerError>>()?;
        Ok(DrainReport { decided })
    }

    async fn balance(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        at: Timestamp,
    ) -> Result<Balance, LedgerError> {
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT o_account_id, o_label, o_available, o_debt FROM ob_balance_of($1, $2, $3)",
        )
        .bind(owner.as_str())
        .bind(currency.as_str())
        .bind(at.unix_micros())
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        let debt = rows.first().map_or(0, |row| row.3);
        let mut available = Amount::ZERO;
        let mut accounts = Vec::with_capacity(rows.len());
        for (account, label, micros, _) in rows {
            let held = amount(micros)?;
            available = available.checked_add(held).unwrap_or(Amount::MAX);
            accounts.push(AccountBalance {
                account: AccountId::new(account).map_err(|_| corrupt())?,
                label: AccountLabel::new(label).map_err(|_| corrupt())?,
                available: held,
            });
        }
        Ok(Balance {
            accounts,
            available,
            debt: amount(debt)?,
        })
    }

    async fn allowed(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        member: &MemberId,
        at: Timestamp,
    ) -> Result<Allowed, LedgerError> {
        let (refused, balance, remaining): (Option<String>, i64, Option<i64>) = sqlx::query_as(
            "SELECT o_refused, o_balance, o_member_remaining FROM ob_allowed($1, $2, $3, $4)",
        )
        .bind(owner.as_str())
        .bind(currency.as_str())
        .bind(member.as_str())
        .bind(at.unix_micros())
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        let refused = match refused.as_deref() {
            None => None,
            Some("balance") => Some(NotAllowed::Balance),
            Some("member_cap") => Some(NotAllowed::MemberCap),
            Some(_) => return Err(corrupt()),
        };
        Ok(Allowed {
            refused,
            balance: amount(balance)?,
            member_remaining: remaining.map(amount).transpose()?,
        })
    }

    async fn lots(&self, account: &AccountId) -> Result<Vec<LotView>, LedgerError> {
        let rows: Vec<LotRow> = sqlx::query_as(
            "SELECT o_kind, o_credit_id, o_month_day, o_amount, o_remaining, o_expires_micros \
                 FROM ob_lots_of($1)",
        )
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        rows.into_iter()
            .map(|(kind, credit, month, total, remaining, expires)| {
                Ok(LotView {
                    kind: LotKind::parse(&kind).ok_or_else(corrupt)?,
                    credit: credit
                        .map(CreditId::new)
                        .transpose()
                        .map_err(|_| corrupt())?,
                    month: month.map(day).transpose()?.map(UtcDay::month),
                    amount: amount(total)?,
                    remaining: amount(remaining)?,
                    expires_at: expires
                        .map(Timestamp::from_unix_micros)
                        .transpose()
                        .map_err(|_| corrupt())?,
                })
            })
            .collect()
    }

    async fn outcome(&self, event: &EventId) -> Result<Option<OutcomeView>, LedgerError> {
        let row: Option<(i32, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT o_occurred_day, o_state, o_reason, o_amount FROM ob_outcome_of($1)",
        )
        .bind(event.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(ledger_error)?;
        row.map(|(occurred_on, state, reason, micros)| {
            Ok(OutcomeView {
                occurred_on: day(occurred_on)?,
                outcome: outcome(&state, reason.as_deref())?,
                amount: amount(micros)?,
            })
        })
        .transpose()
    }

    async fn daily_usage(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
    ) -> Result<Vec<(UtcDay, Amount)>, LedgerError> {
        let rows: Vec<(i32, i64)> =
            sqlx::query_as("SELECT o_day, o_amount FROM ob_daily_usage($1, $2)")
                .bind(owner.as_str())
                .bind(currency.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(ledger_error)?;
        rows.into_iter()
            .map(|(d, micros)| Ok((day(d)?, amount(micros)?)))
            .collect()
    }
}
