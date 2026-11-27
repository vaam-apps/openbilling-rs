//! The procedure bodies of `schema/openbilling.cstack`.
//!
//! The generated router has authenticated the caller, checked the procedure's `@allow` and validated the
//! arguments before a body runs. A read forwards to its `query` block, which checks its own `@allow` again and runs
//! the SQL. A write calls the ledger's PostgreSQL function through [`PgLedger`] (agent guide, rule 12), or the
//! payment provider and then the ledger (ADR 0003); no rule of the ledger is written here.

use std::sync::Arc;

use cratestack::{BigInt, CratestackContext, CratestackError};
use openbilling_core::{
    AccountId, AccountLabel, Amount, CreditId, CreditSource, CreditTarget, Credited, CurrencyCode,
    IdempotencyKey, LedgerConfig, LedgerStore, LotKind as CoreLotKind, LotOrder, NewAccount,
    OwnerId, PaymentProvider, PaymentRequest, PaymentTarget, ReturnUrls, Settled, WithinKind,
    settle_payment, settle_refund,
};

use crate::errors::{invalid, ledger, payment, settle};
use crate::generated::cratestack_schema::{
    AccountOpened, Cratestack, CreditBooked, Done, LotKind, OperatorCreditSource, Settlement,
    TopUpStarted, procedures, queries,
};
use crate::intake;
use crate::time::timestamp;

/// What the procedures call: the ledger (in the service, [`PgLedger`](openbilling_pg::PgLedger)), and the
/// payment provider when one is configured.
pub struct Services<L, P> {
    /// The ledger.
    pub ledger: L,
    /// The payment provider, or `None`: top-ups and reconciliation then answer `503`.
    pub provider: Option<P>,
}

/// The procedure registry.
pub struct Procedures<L, P>(pub Arc<Services<L, P>>);

impl<L, P> Clone for Procedures<L, P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

fn owner(value: &str) -> Result<OwnerId, CratestackError> {
    OwnerId::new(value).map_err(|e| invalid("owner", &e))
}

fn currency(value: &str) -> Result<CurrencyCode, CratestackError> {
    CurrencyCode::new(value).map_err(|e| invalid("currency", &e))
}

fn label(value: &str) -> Result<AccountLabel, CratestackError> {
    AccountLabel::new(value).map_err(|e| invalid("label", &e))
}

fn micros(value: BigInt, field: &str) -> Result<Amount, CratestackError> {
    Amount::from_micros(value.get()).map_err(|e| invalid(field, &e))
}

fn booked(credited: Credited) -> CreditBooked {
    CreditBooked {
        accountId: credited.account.into_string(),
        transaction: credited.transaction,
        replayed: credited.replayed,
        repaidMicros: BigInt::new(credited.repaid.micros()),
        debtAddedMicros: BigInt::new(credited.debt_added.micros()),
    }
}

pub(crate) fn settlement(settled: &Settled) -> Settlement {
    let credited = match settled {
        Settled::Credited(credited) | Settled::Reversed(credited) => Some(credited),
        _ => None,
    };
    Settlement {
        outcome: settled.as_str().to_owned(),
        accountId: credited.map(|c| c.account.as_str().to_owned()),
        transaction: credited.map(|c| c.transaction.clone()),
        replayed: credited.is_some_and(|c| c.replayed),
    }
}

impl<L, P> Procedures<L, P> {
    fn provider(&self) -> Result<&P, CratestackError> {
        self.0.provider.as_ref().ok_or_else(|| {
            CratestackError::Unavailable("no payment provider is configured".to_owned())
        })
    }
}

impl<L, P> procedures::ProcedureRegistry for Procedures<L, P>
where
    L: LedgerStore + 'static,
    P: PaymentProvider + 'static,
{
    async fn get_balance(
        &self,
        db: &Cratestack,
        ctx: &CratestackContext,
        args: procedures::get_balance::Args,
        _authorized: procedures::get_balance::Authorized,
    ) -> Result<procedures::get_balance::Output, CratestackError> {
        let args = queries::balance_rows::Args {
            owner: args.args.owner,
            currency: args.args.currency,
        };
        db.queries().balance_rows(&args, ctx).await
    }

    async fn get_allowed(
        &self,
        db: &Cratestack,
        ctx: &CratestackContext,
        args: procedures::get_allowed::Args,
        _authorized: procedures::get_allowed::Authorized,
    ) -> Result<procedures::get_allowed::Output, CratestackError> {
        let args = queries::allowed_row::Args {
            owner: args.args.owner,
            currency: args.args.currency,
            member: args.args.member,
        };
        db.queries().allowed_row(&args, ctx).await
    }

    async fn list_credits(
        &self,
        db: &Cratestack,
        ctx: &CratestackContext,
        args: procedures::list_credits::Args,
        _authorized: procedures::list_credits::Authorized,
    ) -> Result<procedures::list_credits::Output, CratestackError> {
        let args = queries::credit_rows::Args {
            owner: args.args.owner,
            currency: args.args.currency,
        };
        db.queries().credit_rows(&args, ctx).await
    }

    async fn get_statement(
        &self,
        db: &Cratestack,
        ctx: &CratestackContext,
        args: procedures::get_statement::Args,
        _authorized: procedures::get_statement::Authorized,
    ) -> Result<procedures::get_statement::Output, CratestackError> {
        let args = queries::statement_lines::Args {
            owner: args.args.owner,
            currency: args.args.currency,
            year: args.args.year,
            month: args.args.month,
        };
        db.queries().statement_lines(&args, ctx).await
    }

    async fn get_usage_series(
        &self,
        db: &Cratestack,
        ctx: &CratestackContext,
        args: procedures::get_usage_series::Args,
        _authorized: procedures::get_usage_series::Authorized,
    ) -> Result<procedures::get_usage_series::Output, CratestackError> {
        let args = queries::usage_days::Args {
            owner: args.args.owner,
            currency: args.args.currency,
            fromTime: args.args.fromTime,
            toTime: args.args.toTime,
        };
        db.queries().usage_days(&args, ctx).await
    }

    async fn configure(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::configure::Args,
        _authorized: procedures::configure::Authorized,
    ) -> Result<procedures::configure::Output, CratestackError> {
        let args = args.args;
        let config = LedgerConfig {
            currencies: args
                .currencies
                .iter()
                .map(|c| currency(c))
                .collect::<Result<_, _>>()?,
            labels: args
                .labels
                .iter()
                .map(|l| label(l))
                .collect::<Result<_, _>>()?,
            lot_order: LotOrder(
                args.lotOrder
                    .iter()
                    .map(|entry| {
                        let kind = match entry.kind {
                            LotKind::allowance => CoreLotKind::Allowance,
                            LotKind::promotion => CoreLotKind::Promotion,
                            LotKind::adjustment => CoreLotKind::Adjustment,
                            LotKind::payment => CoreLotKind::Payment,
                        };
                        let within = if entry.soonestExpiryFirst {
                            WithinKind::SoonestExpiryFirst
                        } else {
                            WithinKind::OldestFirst
                        };
                        (kind, within)
                    })
                    .collect(),
            ),
        };
        self.0
            .ledger
            .configure(&config)
            .await
            .map_err(|e| ledger(&e))?;
        Ok(Done { done: true })
    }

    async fn open_account(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::open_account::Args,
        _authorized: procedures::open_account::Authorized,
    ) -> Result<procedures::open_account::Output, CratestackError> {
        let args = args.args;
        let account = NewAccount {
            id: AccountId::new(args.accountId.as_str()).map_err(|e| invalid("accountId", &e))?,
            owner: owner(&args.owner)?,
            currency: currency(&args.currency)?,
            label: label(&args.label)?,
        };
        let opened = self
            .0
            .ledger
            .open_account(&account)
            .await
            .map_err(|e| ledger(&e))?;
        Ok(AccountOpened {
            accountId: account.id.into_string(),
            replayed: opened.replayed,
        })
    }

    async fn grant_credit(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::grant_credit::Args,
        _authorized: procedures::grant_credit::Authorized,
    ) -> Result<procedures::grant_credit::Output, CratestackError> {
        let args = args.args;
        // A payment credit is not on offer here: it comes only from the payment provider (ADR 0003).
        let source = match args.source {
            OperatorCreditSource::promotion => CreditSource::Promotion,
            OperatorCreditSource::adjustment => CreditSource::Adjustment,
        };
        let credit = openbilling_core::Credit {
            source,
            id: CreditId::new(args.creditId.as_str()).map_err(|e| invalid("creditId", &e))?,
            to: CreditTarget::Label {
                owner: owner(&args.owner)?,
                currency: currency(&args.currency)?,
                label: label(&args.label)?,
            },
            amount: micros(args.amountMicros, "amountMicros")?,
            expires_at: args
                .expiresAt
                .as_ref()
                .map(|at| timestamp(at, "expiresAt"))
                .transpose()?,
        };
        let credited = self
            .0
            .ledger
            .credit(&credit)
            .await
            .map_err(|e| ledger(&e))?;
        Ok(booked(credited))
    }

    async fn record_usage(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::record_usage::Args,
        _authorized: procedures::record_usage::Authorized,
    ) -> Result<procedures::record_usage::Output, CratestackError> {
        let events = intake::events(&args.args)?;
        let report = self
            .0
            .ledger
            .intake(&events)
            .await
            .map_err(|e| ledger(&e))?;
        Ok(intake::answer(&report))
    }

    async fn start_top_up(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::start_top_up::Args,
        _authorized: procedures::start_top_up::Authorized,
    ) -> Result<procedures::start_top_up::Output, CratestackError> {
        let provider = self.provider()?;
        let args = args.args;
        let request = PaymentRequest {
            idempotency_key: IdempotencyKey::new(args.key.as_str()).ok_or_else(|| {
                CratestackError::Validation(
                    "key: it holds a character a key does not allow".to_owned(),
                )
            })?,
            to: PaymentTarget {
                owner: owner(&args.owner)?,
                label: label(&args.label)?,
            },
            currency: currency(&args.currency)?,
            amount: micros(args.amountMicros, "amountMicros")?,
            return_to: ReturnUrls {
                success: args.successUrl,
                cancel: args.cancelUrl,
            },
        };
        let created = provider
            .create_payment(&request)
            .await
            .map_err(|e| payment(&e))?;
        Ok(TopUpStarted {
            payment: created.payment.into_string(),
            status: created.status.as_str().to_owned(),
            checkoutId: created.checkout.id,
            checkoutUrl: created.checkout.url,
        })
    }

    async fn reconcile_payment(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::reconcile_payment::Args,
        _authorized: procedures::reconcile_payment::Authorized,
    ) -> Result<procedures::reconcile_payment::Output, CratestackError> {
        let provider = self.provider()?;
        let id = CreditId::new(args.args.id.as_str()).map_err(|e| invalid("id", &e))?;
        let settled = settle_payment(&self.0.ledger, provider, &id)
            .await
            .map_err(|e| settle(&e))?;
        Ok(settlement(&settled))
    }

    async fn reconcile_refund(
        &self,
        _db: &Cratestack,
        _ctx: &CratestackContext,
        args: procedures::reconcile_refund::Args,
        _authorized: procedures::reconcile_refund::Authorized,
    ) -> Result<procedures::reconcile_refund::Output, CratestackError> {
        let provider = self.provider()?;
        let id = CreditId::new(args.args.id.as_str()).map_err(|e| invalid("id", &e))?;
        let settled = settle_refund(&self.0.ledger, provider, &id)
            .await
            .map_err(|e| settle(&e))?;
        Ok(settlement(&settled))
    }
}
