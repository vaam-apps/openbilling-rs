//! Settling with a payment provider: a payment read back as succeeded becomes a `payment` credit, and a refund read
//! back as succeeded becomes a reversal (ADR 0003).
//!
//! This is composition of the two ports and nothing else. Whether a credit is once-only, which account it reaches
//! and how much a reversal may take back are the ledger's rules, in SQL (agent guide, rule 12): the payment's
//! provider id is the credit's once-only id, so settling the same payment twice, from a webhook and from a
//! reconciliation at once, books it once.

use crate::config::{CreditSource, ReversalKind};
use crate::error::{Classify, ErrorClass};
use crate::id::CreditId;
use crate::payment::{PaymentError, PaymentProvider, PaymentStatus, RefundStatus, WebhookHint};
use crate::refusal::LedgerError;
use crate::store::{Credit, CreditTarget, Credited, LedgerStore, Reversal};

/// What settling did.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settled {
    /// The payment succeeded and is credited: now, or already (`replayed`).
    Credited(Credited),
    /// The refund succeeded and is reversed: now, or already (`replayed`).
    Reversed(Credited),
    /// The payment is not paid yet, or never will be: nothing to book.
    PaymentNotSettled(PaymentStatus),
    /// The refund has not given the money back, or never will: nothing to book.
    RefundNotSettled(RefundStatus),
    /// A payment openbilling did not ask for, or a refund of one: it moves no credit here.
    NotOurs,
    /// A webhook about nothing that moves credit.
    Ignored,
}

impl Settled {
    /// The outcome's stable name.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Credited(_) => "credited",
            Self::Reversed(_) => "reversed",
            Self::PaymentNotSettled(_) => "payment_not_settled",
            Self::RefundNotSettled(_) => "refund_not_settled",
            Self::NotOurs => "not_ours",
            Self::Ignored => "ignored",
        }
    }
}

/// Why settling failed: the provider's error or the ledger's. Nothing was booked.
#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    /// Reading the provider failed.
    #[error(transparent)]
    Provider(#[from] PaymentError),
    /// The ledger refused or failed.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl Classify for SettleError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Provider(error) => error.class(),
            Self::Ledger(error) => error.class(),
        }
    }
}

/// Reads the payment `id` back and, when it succeeded and openbilling asked for it, books it as a `payment` credit
/// keyed by its provider id, to the owner's account in the payment's currency with the payment's label.
///
/// # Errors
///
/// [`SettleError`] when the provider cannot be read or the ledger refuses (for example `unknown_account`: the owner
/// has no account with that label in the payment's currency).
pub async fn settle_payment<L, P>(
    ledger: &L,
    provider: &P,
    id: &CreditId,
) -> Result<Settled, SettleError>
where
    L: LedgerStore,
    P: PaymentProvider,
{
    let payment = provider.read_payment(id).await?;
    if payment.status != PaymentStatus::Succeeded {
        return Ok(Settled::PaymentNotSettled(payment.status));
    }
    let Some(to) = payment.to else {
        return Ok(Settled::NotOurs);
    };
    let credited = ledger
        .credit(&Credit {
            source: CreditSource::Payment,
            id: payment.id,
            to: CreditTarget::Label {
                owner: to.owner,
                currency: payment.currency,
                label: to.label,
            },
            amount: payment.amount,
            expires_at: None,
        })
        .await?;
    Ok(Settled::Credited(credited))
}

/// Reads the refund `id` back and, when it gave the money back, books it as a reversal of its payment, keyed by its
/// provider id. The payment is settled first, so a refund that arrives before its payment's webhook still finds
/// the credit it reverses; the ledger caps what the reversals of one payment take back.
///
/// # Errors
///
/// [`SettleError`] when the provider cannot be read or the ledger refuses (for example `reversal_cap`).
pub async fn settle_refund<L, P>(
    ledger: &L,
    provider: &P,
    id: &CreditId,
) -> Result<Settled, SettleError>
where
    L: LedgerStore,
    P: PaymentProvider,
{
    let refund = provider.read_refund(id).await?;
    if refund.status != RefundStatus::Succeeded {
        return Ok(Settled::RefundNotSettled(refund.status));
    }
    match settle_payment(ledger, provider, &refund.payment).await? {
        Settled::Credited(_) => {}
        Settled::NotOurs => return Ok(Settled::NotOurs),
        // A refund that succeeded of a payment that did not: the provider contradicts itself. Book nothing.
        other @ (Settled::PaymentNotSettled(_)
        | Settled::Reversed(_)
        | Settled::RefundNotSettled(_)
        | Settled::Ignored) => return Ok(other),
    }
    let reversed = ledger
        .reverse(&Reversal {
            kind: ReversalKind::Refund,
            id: refund.id,
            payment: refund.payment,
            amount: refund.amount,
        })
        .await?;
    Ok(Settled::Reversed(reversed))
}

/// Settles what a verified webhook names.
///
/// # Errors
///
/// As [`settle_payment`] and [`settle_refund`].
pub async fn settle_hint<L, P>(
    ledger: &L,
    provider: &P,
    hint: &WebhookHint,
) -> Result<Settled, SettleError>
where
    L: LedgerStore,
    P: PaymentProvider,
{
    match hint {
        WebhookHint::Payment(id) => settle_payment(ledger, provider, id).await,
        WebhookHint::Refund(id) => settle_refund(ledger, provider, id).await,
        WebhookHint::Ignored => Ok(Settled::Ignored),
    }
}
