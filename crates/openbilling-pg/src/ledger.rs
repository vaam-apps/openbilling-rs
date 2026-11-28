//! [`PgLedger`]: the [`LedgerStore`] port on PostgreSQL.
//!
//! A thin caller (agent guide, rule 12): each write is one statement that calls one function of the migrations, in
//! its own transaction, and maps the answer to the port's types; a read that carries no rule is a plain query. No
//! rule of the ledger is written here. It also implements [`PriceBook`] (ADR 0004).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use openbilling_core::{
    AccountBalance, AccountId, AccountLabel, Allowed, AllowedPolicy, Amount, Balance, CreditId,
    CreditTarget, Credited, CurrencyCode, Decided, DrainReport, EventId, EventOutcome, Intake,
    IntakeReport, LedgerConfig, LedgerError, LedgerStore, Loaded, LotKind, LotView, MemberId,
    MismatchField, NewAccount, NotAllowed, Opened, OutcomeView, OwnerId, PartitionRetention,
    PaymentView, PriceBook, PriceVersionId, PriceVersionView, Rated, RawEvent, ReconcileSettings,
    RefusalReason, RetentionMargin, RetentionReport, Reversal, Timestamp, TopUp, TopUpLedger,
    TopUpSettled, UnrecordedTopUps, UsageEvent, UtcDay, UtcMonth, Window, WithinKind,
};
use openbilling_metrics::{Counter, Gauge, Histogram, global};
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
    /// How long after the window a day's partition is dropped (ADR 0001, point 10, amended 2026-11-28); the deployer's
    /// choice, default 90 days. Used by [`PgLedger::run_retention`].
    pub retention_margin: RetentionMargin,
    /// The longest one retention run may wait for a lock (a detach waits for every transaction on the table) or run a
    /// statement, as `lock_timeout` and `statement_timeout` of its connection. The deployer's, default 5 minutes.
    pub retention_timeout: Duration,
    /// What to do with a succeeded payment no top-up was recorded for (ADR 0003, amended 2026-11-28): the deployer's
    /// `payments.unrecorded`, default [`UnrecordedTopUps::Refuse`]. Passed to the SQL as an argument.
    pub unrecorded_top_ups: UnrecordedTopUps,
}

impl Default for LedgerSettings {
    fn default() -> Self {
        Self {
            window: Window::default(),
            partitions_ahead_days: 2,
            notify_interval: Duration::from_millis(100),
            retention_margin: RetentionMargin::default(),
            retention_timeout: Duration::from_secs(300),
            unrecorded_top_ups: UnrecordedTopUps::Refuse,
        }
    }
}

/// What the usage queue holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueStats {
    /// Events received and not yet decided.
    pub backlog: u64,
    /// The age of the oldest of them; zero with none.
    pub oldest_age: Duration,
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
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "prepare_partitions"))]
    pub async fn prepare_partitions(&self) -> Result<u32, LedgerError> {
        let made: i32 = sqlx::query_scalar("SELECT ob_prepare_partitions($1, $2)")
            .bind(i32::from(self.settings.window.days()))
            .bind(i32::from(self.settings.partitions_ahead_days))
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        u32::try_from(made).map_err(|_| corrupt())
    }

    /// Runs the retention job with the margin of the settings. See [`PartitionRetention::drop_expired_partitions`].
    ///
    /// # Errors
    ///
    /// A [`LedgerError::Store`] when the database fails.
    pub async fn run_retention(&self) -> Result<RetentionReport, LedgerError> {
        self.drop_expired_partitions(self.settings.retention_margin)
            .await
    }

    /// What the queue holds now: how many events are received and not yet decided, and how old the oldest is.
    ///
    /// # Errors
    ///
    /// A [`LedgerError::Store`] when the database fails.
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "queue_stats"))]
    pub async fn queue_stats(&self) -> Result<QueueStats, LedgerError> {
        let (backlog, age): (i64, Option<f64>) = sqlx::query_as(
            "SELECT count(*)::bigint, extract(epoch FROM now() - min(received_at))::float8 \
             FROM ob_usage_event WHERE state = 'received'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        Ok(QueueStats {
            backlog: u64::try_from(backlog).map_err(|_| corrupt())?,
            oldest_age: Duration::from_secs_f64(age.unwrap_or(0.0).max(0.0)),
        })
    }

    /// Sets the pool and queue gauges of the metrics: connections idle and in use, the pool's maximum, the queue's
    /// backlog and the age of its oldest event. The pool's gauges need no query; the queue's is one. A database
    /// that does not answer leaves the queue gauges as they were: the errors counter and the drain's lag say so.
    pub async fn sample_metrics(&self) {
        let metrics = global();
        let size = self.pool.size() as usize;
        let idle = self.pool.num_idle();
        #[allow(clippy::cast_precision_loss)]
        {
            metrics.set(Gauge::PoolConnections, &[("state", "idle")], idle as f64);
            metrics.set(
                Gauge::PoolConnections,
                &[("state", "used")],
                size.saturating_sub(idle) as f64,
            );
            metrics.set(
                Gauge::PoolMax,
                &[],
                f64::from(self.pool.options().get_max_connections()),
            );
        }
        if let Ok(queue) = self.queue_stats().await {
            #[allow(clippy::cast_precision_loss)]
            metrics.set(Gauge::QueueBacklog, &[], queue.backlog as f64);
            metrics.set(Gauge::QueueOldestAge, &[], queue.oldest_age.as_secs_f64());
        }
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

/// A decision as `ob_drain` returns it: event id, occurrence day, state, reason, amount, price book version.
type DrainRow = (String, i32, String, Option<String>, i64, Option<String>);

/// An outcome as `PgLedger::outcome` reads it: occurrence day, state, reason, amount, price book version.
type OutcomeRow = (i32, String, Option<String>, i64, Option<String>);

/// A row of `ob_allowed`: the verdict, the net balance, what the lots hold, the debt, the limit, the flag, the
/// member's usage, cap and remainder.
type AllowedRow = (
    Option<String>,
    i64,
    i64,
    i64,
    Option<i64>,
    bool,
    i64,
    Option<i64>,
    Option<i64>,
);

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

fn version(id: Option<String>) -> Result<Option<PriceVersionId>, LedgerError> {
    id.map(PriceVersionId::new)
        .transpose()
        .map_err(|_| corrupt())
}

fn timestamp(micros: i64) -> Result<Timestamp, LedgerError> {
    Timestamp::from_unix_micros(micros).map_err(|_| corrupt())
}

/// One batch for `ob_intake`: priced rows carry an amount and no meter, raw rows a meter, a quantity and their
/// dimensions (by 1-based position) and no amount.
#[derive(Default)]
struct IntakeBatch<'a> {
    ids: Vec<&'a str>,
    owners: Vec<&'a str>,
    currencies: Vec<&'a str>,
    members: Vec<&'a str>,
    amounts: Vec<Option<i64>>,
    occurred: Vec<i64>,
    meters: Vec<Option<&'a str>>,
    quantities: Vec<Option<i64>>,
    dimension_of: Vec<i32>,
    dimension_keys: Vec<&'a str>,
    dimension_values: Vec<&'a str>,
}

impl<'a> IntakeBatch<'a> {
    fn priced(events: &'a [UsageEvent]) -> Self {
        let mut batch = Self::default();
        for e in events {
            batch.row(
                e.id.as_str(),
                e.owner.as_str(),
                e.currency.as_str(),
                e.member.as_str(),
            );
            batch.amounts.push(Some(e.amount.micros()));
            batch.occurred.push(e.occurred_at.unix_micros());
            batch.meters.push(None);
            batch.quantities.push(None);
        }
        batch
    }

    fn raw(events: &'a [RawEvent]) -> Self {
        let mut batch = Self::default();
        for (n, e) in events.iter().enumerate() {
            batch.row(
                e.id.as_str(),
                e.owner.as_str(),
                e.currency.as_str(),
                e.member.as_str(),
            );
            batch.amounts.push(None);
            batch.occurred.push(e.occurred_at.unix_micros());
            batch.meters.push(Some(e.meter.as_str()));
            batch.quantities.push(Some(e.quantity.units()));
            // At most 1000 events reach the SQL; a longer batch is refused there, so the position never saturates
            // where it matters.
            let position = i32::try_from(n + 1).unwrap_or(i32::MAX);
            for (key, value) in e.dimensions.iter() {
                batch.dimension_of.push(position);
                batch.dimension_keys.push(key.as_str());
                batch.dimension_values.push(value.as_str());
            }
        }
        batch
    }

    fn row(&mut self, id: &'a str, owner: &'a str, currency: &'a str, member: &'a str) {
        self.ids.push(id);
        self.owners.push(owner);
        self.currencies.push(currency);
        self.members.push(member);
    }
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

/// Counts a drain pass: its result, its duration, each decision, and a debit posting for each applied event.
fn record_drain(result: &Result<DrainReport, LedgerError>, began: Instant) {
    let metrics = global();
    metrics.observe(Histogram::Drain, &[], began.elapsed().as_secs_f64());
    match result {
        Ok(report) => {
            metrics.add(Counter::DrainRuns, &[("result", "ok")], 1);
            metrics.drain_succeeded();
            let applied = report.applied() as u64;
            metrics.add(Counter::DrainEvents, &[("outcome", "applied")], applied);
            metrics.add(Counter::LedgerPostings, &[("kind", "usage_debit")], applied);
            for reason in RefusalReason::ALL {
                let refused = report
                    .decided
                    .iter()
                    .filter(|d| d.outcome == EventOutcome::Refused(reason))
                    .count() as u64;
                metrics.add(
                    Counter::DrainEvents,
                    &[("outcome", "refused"), ("reason", reason.as_str())],
                    refused,
                );
            }
        }
        Err(_) => metrics.add(Counter::DrainRuns, &[("result", "error")], 1),
    }
}

/// Counts a booked credit, reversal or allowance as a ledger posting; a replay booked nothing.
fn posted(credited: Credited, kind: &'static str) -> Result<Credited, LedgerError> {
    if !credited.replayed {
        global().add(Counter::LedgerPostings, &[("kind", kind)], 1);
    }
    Ok(credited)
}

impl PgLedger {
    /// Calls `ob_intake` with one batch, priced or raw, and notifies the workers when it wrote anything.
    async fn take_in(&self, batch: IntakeBatch<'_>) -> Result<IntakeReport, LedgerError> {
        let events = batch.ids.len() as u64;
        let result = self.take_in_rows(batch).await;
        let metrics = global();
        match &result {
            Ok(report) => {
                metrics.add(
                    Counter::IntakeEvents,
                    &[("result", "accepted")],
                    report.accepted() as u64,
                );
                metrics.add(
                    Counter::IntakeEvents,
                    &[("result", "replayed")],
                    report.replayed() as u64,
                );
            }
            Err(error) => {
                if let Some(rule) = error.rule() {
                    metrics.add(
                        Counter::IntakeEvents,
                        &[("result", "refused"), ("reason", rule.as_str())],
                        events,
                    );
                }
            }
        }
        result
    }

    async fn drain_rows(&self, limit: u32) -> Result<DrainReport, LedgerError> {
        let rows: Vec<DrainRow> = sqlx::query_as(
            "SELECT o_event_id, o_occurred_day, o_state, o_reason, o_amount, o_price_version::text \
             FROM ob_drain($1)",
        )
        .bind(i32::try_from(limit).unwrap_or(i32::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        let decided = rows
            .into_iter()
            .map(|(id, occurred_on, state, reason, micros, price_version)| {
                Ok(Decided {
                    event: EventId::new(id).map_err(|_| corrupt())?,
                    occurred_on: day(occurred_on)?,
                    outcome: outcome(&state, reason.as_deref())?,
                    amount: amount(micros)?,
                    price_version: version(price_version)?,
                })
            })
            .collect::<Result<Vec<_>, LedgerError>>()?;
        Ok(DrainReport { decided })
    }

    async fn take_in_rows(&self, batch: IntakeBatch<'_>) -> Result<IntakeReport, LedgerError> {
        let window = self.settings.window;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT o_event_id, o_answer \
             FROM ob_intake($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(batch.ids)
        .bind(batch.owners)
        .bind(batch.currencies)
        .bind(batch.members)
        .bind(batch.amounts)
        .bind(batch.occurred)
        .bind(batch.meters)
        .bind(batch.quantities)
        .bind(batch.dimension_of)
        .bind(batch.dimension_keys)
        .bind(batch.dimension_values)
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
}

impl LedgerStore for PgLedger {
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "now"))]
    async fn now(&self) -> Result<Timestamp, LedgerError> {
        let micros: i64 = sqlx::query_scalar("SELECT ob_micros(now())")
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        Timestamp::from_unix_micros(micros).map_err(|_| corrupt())
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "configure"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "open_account"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "set_debt_limit"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "set_member_cap"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "grant_allowance"))]
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
        posted(credited(row)?, "allowance")
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "credit"))]
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
        posted(credited(row)?, "credit")
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "reverse"))]
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
        posted(credited(row)?, "reversal")
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "intake"))]
    async fn intake(&self, events: &[UsageEvent]) -> Result<IntakeReport, LedgerError> {
        self.take_in(IntakeBatch::priced(events)).await
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "drain", openbilling.events.decided = tracing::field::Empty))]
    async fn drain(&self, limit: u32) -> Result<DrainReport, LedgerError> {
        let began = Instant::now();
        let result = self.drain_rows(limit).await;
        record_drain(&result, began);
        if let Ok(report) = &result {
            tracing::Span::current()
                .record("openbilling.events.decided", report.decided.len() as u64);
        }
        result
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "balance"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "allowed"))]
    async fn allowed(
        &self,
        owner: &OwnerId,
        currency: &CurrencyCode,
        member: &MemberId,
        at: Timestamp,
        policy: AllowedPolicy,
    ) -> Result<Allowed, LedgerError> {
        let row: AllowedRow = sqlx::query_as(
            "SELECT o_refused, o_balance, o_available, o_debt, o_debt_limit, o_past_limit, o_member_spent, \
                    o_member_cap, o_member_remaining \
                 FROM ob_allowed($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(owner.as_str())
        .bind(currency.as_str())
        .bind(member.as_str())
        .bind(at.unix_micros())
        .bind(policy.debt_limit.as_str())
        .bind(policy.balance.as_str())
        .bind(policy.member_cap.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        let (refused, balance, available, debt, debt_limit, past_limit, spent, cap, remaining) =
            row;
        let refused = match refused.as_deref() {
            None => None,
            Some("debt_limit") => Some(NotAllowed::DebtLimit),
            Some("balance") => Some(NotAllowed::Balance),
            Some("member_cap") => Some(NotAllowed::MemberCap),
            Some(_) => return Err(corrupt()),
        };
        Ok(Allowed {
            refused,
            balance: amount(balance)?,
            available: amount(available)?,
            debt: amount(debt)?,
            debt_limit: debt_limit.map(amount).transpose()?,
            past_limit,
            member_spent: amount(spent)?,
            member_cap: cap.map(amount).transpose()?,
            member_remaining: remaining.map(amount).transpose()?,
        })
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "lots"))]
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

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "outcome"))]
    async fn outcome(&self, event: &EventId) -> Result<Option<OutcomeView>, LedgerError> {
        // A plain read: no rule, so no function (the owner's decision of 2026-11-27).
        let row: Option<OutcomeRow> = sqlx::query_as(
            "SELECT ob_day(o.occurred_on), o.state, o.reason, o.amount, o.price_version::text \
             FROM ob_event_outcome o WHERE o.event_id = $1 AND o.state <> 'pending'",
        )
        .bind(event.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(ledger_error)?;
        row.map(|(occurred_on, state, reason, micros, price_version)| {
            Ok(OutcomeView {
                occurred_on: day(occurred_on)?,
                outcome: outcome(&state, reason.as_deref())?,
                amount: amount(micros)?,
                price_version: version(price_version)?,
            })
        })
        .transpose()
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "daily_usage"))]
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

impl PriceBook for PgLedger {
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "load_price_list"))]
    async fn load_price_list(&self, yaml: &str) -> Result<Loaded, LedgerError> {
        let list = crate::price_list::parse(yaml)?;
        let (id, effective_micros, content_hash, replayed): (String, i64, String, bool) =
            sqlx::query_as(
                "SELECT o_version_id::text, o_effective_micros, o_content_hash, o_replayed \
                 FROM ob_load_price_list($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
            )
            .bind(list.effective_from)
            .bind(list.top)
            .bind(list.meter_names)
            .bind(list.meter_units)
            .bind(list.meter_lines)
            .bind(list.meter_columns)
            .bind(list.meter_dimension_of)
            .bind(list.meter_dimensions)
            .bind(list.price_meters)
            .bind(list.price_currencies)
            .bind(list.price_micros)
            .bind(list.price_per)
            .bind(list.price_lines)
            .bind(list.price_columns)
            .bind(list.price_dimension_of)
            .bind(list.price_dimension_keys)
            .bind(list.price_dimension_values)
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)?;
        Ok(Loaded {
            version: PriceVersionId::new(id).map_err(|_| corrupt())?,
            effective_from: timestamp(effective_micros)?,
            content_hash,
            replayed,
        })
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "price_versions"))]
    async fn price_versions(&self) -> Result<Vec<PriceVersionView>, LedgerError> {
        // A plain read: no rule, so no function (the owner's decision of 2026-11-27).
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT v.version_id::text, ob_micros(v.effective_from), encode(v.content_hash, 'hex') \
             FROM ob_price_version v ORDER BY v.effective_from",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(ledger_error)?;
        rows.into_iter()
            .map(|(id, micros, content_hash)| {
                Ok(PriceVersionView {
                    version: PriceVersionId::new(id).map_err(|_| corrupt())?,
                    effective_from: timestamp(micros)?,
                    content_hash,
                })
            })
            .collect()
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "rate"))]
    async fn rate(&self, event: &RawEvent) -> Result<Rated, LedgerError> {
        let keys: Vec<&str> = event.dimensions.iter().map(|(k, _)| k.as_str()).collect();
        let values: Vec<&str> = event.dimensions.iter().map(|(_, v)| v.as_str()).collect();
        // A function, not a query: the refusals of an unpriced meter or dimension are SQLSTATEs, which one plain
        // statement cannot raise.
        let (micros, id): (i64, String) =
            sqlx::query_as("SELECT o_amount, o_version::text FROM ob_rate($1, $2, $3, $4, $5, $6)")
                .bind(event.currency.as_str())
                .bind(event.meter.as_str())
                .bind(event.quantity.units())
                .bind(keys)
                .bind(values)
                .bind(event.occurred_at.unix_micros())
                .fetch_one(&self.pool)
                .await
                .map_err(ledger_error)?;
        Ok(Rated {
            amount: amount(micros)?,
            version: PriceVersionId::new(id).map_err(|_| corrupt())?,
        })
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "intake_raw"))]
    async fn intake_raw(&self, events: &[RawEvent]) -> Result<IntakeReport, LedgerError> {
        self.take_in(IntakeBatch::raw(events)).await
    }
}

/// What one retention run did: the days it dropped, how many partitions it failed on, and the first failure.
struct RunOutcome {
    dropped: Vec<UtcDay>,
    failed: usize,
    first_error: Option<LedgerError>,
}

/// Whether `error` is the SQL's own refusal to drop (`OB904`): expected after a finalize, whose partition may still
/// hold an undecided event or be inside the margin.
fn refused_to_drop(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(d) if d.code().as_deref() == Some("OB904"))
}

/// The statements of one retention run, in turn, on one connection: the plan, then per partition the detach (a
/// top-level statement: PostgreSQL refuses `DETACH … CONCURRENTLY` in a transaction block, a function or a procedure)
/// and the drop. Every rule is in the SQL functions; this only calls them in order.
///
/// One partition failing (a lock timeout on its detach, say) does not stop the others: each drop is counted as it
/// happens, and the failures are returned. A detach that failed leaves its partition pending-detach, and PostgreSQL
/// refuses another detach until it is finalized, so the run skips the remaining detaches and still drops the tables
/// that are already detached.
async fn retention_run(
    conn: &mut sqlx::PgConnection,
    window_days: i32,
    margin_days: i32,
) -> Result<RunOutcome, LedgerError> {
    let plan: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT partition_name, action, statement FROM ob_expired_partitions($1, $2)",
    )
    .bind(window_days)
    .bind(margin_days)
    .fetch_all(&mut *conn)
    .await
    .map_err(ledger_error)?;
    let mut out = RunOutcome {
        dropped: Vec::with_capacity(plan.len()),
        failed: 0,
        first_error: None,
    };
    let mut detach_blocked = false;
    for (partition, action, statement) in plan {
        if let Some(statement) = statement {
            if action == "detach" && detach_blocked {
                continue;
            }
            // The statement is built by the SQL function with `format('%I')`. The simple protocol, so nothing is
            // prepared and the command is its own transaction.
            if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
                .execute(&mut *conn)
                .await
            {
                detach_blocked = true;
                out.failed += 1;
                out.first_error.get_or_insert_with(|| ledger_error(error));
                continue;
            }
        }
        let dropped: Result<i32, sqlx::Error> =
            sqlx::query_scalar("SELECT ob_day(ob_drop_partition($1, $2, $3))")
                .bind(&partition)
                .bind(window_days)
                .bind(margin_days)
                .fetch_one(&mut *conn)
                .await;
        match dropped {
            Ok(days) => {
                out.dropped.push(day(days)?);
                global().add(Counter::PartitionsDropped, &[], 1);
            }
            Err(error) if action == "finalize" && refused_to_drop(&error) => {}
            Err(error) => {
                out.failed += 1;
                out.first_error.get_or_insert_with(|| ledger_error(error));
            }
        }
    }
    Ok(out)
}

impl PartitionRetention for PgLedger {
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "drop_expired_partitions"))]
    async fn drop_expired_partitions(
        &self,
        margin: RetentionMargin,
    ) -> Result<RetentionReport, LedgerError> {
        // One connection for the whole run: the lock is the session's, and the detach cannot be in a transaction.
        let mut conn = self.pool.acquire().await.map_err(ledger_error)?;
        // However the run ends, the connection is closed rather than returned to the pool, so the lock cannot
        // outlive the run even if the explicit unlock below fails.
        conn.close_on_drop();
        // A detach waits for every transaction that holds a lock on the table: bound the wait, so a run ends.
        let timeout_ms =
            i64::try_from(self.settings.retention_timeout.as_millis()).unwrap_or(i64::MAX);
        sqlx::query_scalar::<_, String>(
            "SELECT set_config('lock_timeout', $1, false) || set_config('statement_timeout', $1, false)",
        )
        .bind(format!("{timeout_ms}ms"))
        .fetch_one(&mut *conn)
        .await
        .map_err(ledger_error)?;
        let locked: bool = sqlx::query_scalar("SELECT ob_retention_try_lock()")
            .fetch_one(&mut *conn)
            .await
            .map_err(ledger_error)?;
        if !locked {
            return Ok(RetentionReport {
                dropped: Vec::new(),
                skipped: true,
            });
        }
        let ran = retention_run(
            &mut conn,
            i32::from(self.settings.window.days()),
            i32::from(margin.days()),
        )
        .await;
        let _ = sqlx::query_scalar::<_, bool>("SELECT ob_retention_unlock()")
            .fetch_one(&mut *conn)
            .await;
        let ran = ran?;
        if let Some(error) = ran.first_error {
            // What was dropped before the failure is already counted; say so, by number only.
            tracing::warn!(
                "a retention run dropped {} partition(s) and failed on {}",
                ran.dropped.len(),
                ran.failed
            );
            return Err(error);
        }
        Ok(RetentionReport {
            dropped: ran.dropped,
            skipped: false,
        })
    }
}

/// Microseconds of a duration, saturating at the largest `bigint`.
fn micros_of(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

impl TopUpLedger for PgLedger {
    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "record_top_up"))]
    async fn record_top_up(&self, top_up: &TopUp) -> Result<bool, LedgerError> {
        sqlx::query_scalar("SELECT ob_record_top_up($1, $2, $3, $4, $5, $6)")
            .bind(top_up.payment.as_str())
            .bind(top_up.key.as_str())
            .bind(top_up.to.owner.as_str())
            .bind(top_up.currency.as_str())
            .bind(top_up.to.label.as_str())
            .bind(top_up.amount.micros())
            .fetch_one(&self.pool)
            .await
            .map_err(ledger_error)
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "settle_top_up"))]
    #[allow(clippy::type_complexity)] // one row of `ob_settle_top_up`, read once into locals
    async fn settle_top_up(&self, payment: &PaymentView) -> Result<TopUpSettled, LedgerError> {
        let (outcome, field, account, txn, replayed, repaid, debt_added, marked): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            i64,
            i64,
            bool,
        ) = sqlx::query_as(
            "SELECT o_outcome, o_field, o_account_id, o_txn_id::text, o_replayed, o_repaid, o_debt_added, \
                    o_marked \
             FROM ob_settle_top_up($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(payment.id.as_str())
        .bind(payment.status.as_str())
        .bind(payment.to.as_ref().map(|to| to.owner.as_str()))
        .bind(payment.currency.as_str())
        .bind(payment.to.as_ref().map(|to| to.label.as_str()))
        .bind(payment.amount.micros())
        .bind(self.settings.unrecorded_top_ups.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(ledger_error)?;
        match outcome.as_str() {
            "credited" => {
                let (Some(account), Some(txn)) = (account, txn) else {
                    return Err(corrupt());
                };
                Ok(TopUpSettled::Credited(credited((
                    account, txn, replayed, repaid, debt_added,
                ))?))
            }
            "canceled" => Ok(TopUpSettled::Canceled),
            "pending" => Ok(TopUpSettled::Pending),
            "not_ours" => Ok(TopUpSettled::NotOurs),
            "mismatch" => {
                let field = field
                    .as_deref()
                    .and_then(MismatchField::from_name)
                    .ok_or_else(corrupt)?;
                // Once, where the ledger decides: only the call that marked the top-up (`o_marked`) counts and
                // warns, so an answer repeated for a sticky row is not a new mismatch. The field is a closed name,
                // never a value.
                if marked {
                    global().add(Counter::PaymentMismatches, &[("field", field.as_str())], 1);
                    tracing::warn!(
                        openbilling.mismatch.field = field.as_str(),
                        openbilling.error.class = "conflict",
                        "a payment read back differs from its top-up; not credited"
                    );
                }
                Ok(TopUpSettled::Mismatch(field))
            }
            _ => Err(corrupt()),
        }
    }

    #[tracing::instrument(name = "db.query", skip_all, fields(db.system = "postgresql", db.operation = "claim_pending_top_ups"))]
    async fn claim_pending_top_ups(
        &self,
        settings: &ReconcileSettings,
    ) -> Result<Vec<CreditId>, LedgerError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT o_payment_id FROM ob_claim_pending_top_ups($1, $2, $3, $4)")
                .bind(micros_of(settings.pending_after))
                .bind(micros_of(settings.read_again_after))
                .bind(micros_of(settings.stop_after))
                .bind(i32::try_from(settings.batch).unwrap_or(i32::MAX))
                .fetch_all(&self.pool)
                .await
                .map_err(ledger_error)?;
        ids.into_iter()
            .map(|id| CreditId::new(id).map_err(|_| corrupt()))
            .collect()
    }
}
