//! Top-ups (ADR 0003, amended 2026-11-28): what openbilling asked a provider for, kept before the payer is sent
//! away, so that what the provider reports back is compared with it and a paid top-up whose webhook never came is
//! found again.
//!
//! The comparison, the credit and the claim of what to read again are SQL, in `openbilling-pg`'s migrations (agent
//! guide, rule 12). This module names what goes in and out of them, and composes the two ports: [`reconcile_pending`]
//! claims the pending top-ups that are due and settles each by reading it back.

use std::future::Future;
use std::time::Duration;

use crate::id::CreditId;
use crate::payment::{IdempotencyKey, PaymentProvider, PaymentStatus, PaymentTarget, PaymentView};
use crate::refusal::LedgerError;
use crate::settle::{SettleError, Settled, settle_payment};
use crate::store::Credited;
use crate::{Amount, CurrencyCode};

/// The field a payment read back differs in from the top-up that was recorded. A closed set: it names the field, never
/// its value, so an error and a metric label hold nothing a caller sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MismatchField {
    /// The owner reference of the payment is not the owner of the top-up (or is gone).
    Owner,
    /// The label reference of the payment is not the top-up's (or is gone).
    Label,
    /// The currency differs.
    Currency,
    /// The amount differs.
    Amount,
    /// No top-up was recorded for the payment and the deployment refuses to credit one it did not record.
    Unrecorded,
}

impl MismatchField {
    /// Every field, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Owner,
        Self::Label,
        Self::Currency,
        Self::Amount,
        Self::Unrecorded,
    ];

    /// The field's stable name, for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Label => "label",
            Self::Currency => "currency",
            Self::Amount => "amount",
            Self::Unrecorded => "unrecorded",
        }
    }

    /// The field named `name`, or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == name)
    }
}

/// What to do with a succeeded payment no top-up was recorded for: one started before top-ups were recorded, or one
/// the provider holds that openbilling never asked for. The deployer's choice (`payments.unrecorded`); the default,
/// [`Self::Refuse`], fails closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UnrecordedTopUps {
    /// Credit nothing and report a mismatch of the field `unrecorded`.
    #[default]
    Refuse,
    /// Credit it from what the provider reports back, as before top-ups were recorded.
    Credit,
}

impl UnrecordedTopUps {
    /// The policy's stable name, as the configuration writes it and the SQL reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Credit => "credit",
        }
    }
}

/// A top-up as it was asked for: recorded once, right after the provider made the payment and before the payer is sent
/// to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TopUp {
    /// The provider's payment id.
    pub payment: CreditId,
    /// The caller's idempotency key.
    pub key: IdempotencyKey,
    /// The account it credits.
    pub to: PaymentTarget,
    /// The currency.
    pub currency: CurrencyCode,
    /// How much, in micro-units. Positive.
    pub amount: Amount,
}

/// What reading a payment back did to its top-up.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TopUpSettled {
    /// The payment succeeded and the top-up's recorded terms are credited: now, or already (`replayed`).
    Credited(Credited),
    /// The provider says it will never succeed; the top-up is closed.
    Canceled,
    /// Not paid yet; the top-up is still pending.
    Pending,
    /// A payment openbilling did not ask for and did not record, with no owner reference: it moves no credit.
    NotOurs,
    /// What the provider reports differs from what was recorded: nothing is credited, and the top-up stays refused.
    Mismatch(MismatchField),
}

/// When a pending top-up is read back, and how many at a time. The deployer's (`reconcile:`); each value has a
/// documented default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileSettings {
    /// A top-up is not read until it is this old (the payer is probably still paying, and the webhook is on its way).
    pub pending_after: Duration,
    /// A top-up read and still pending is read again after this long.
    pub read_again_after: Duration,
    /// A top-up older than this is no longer read: the payment is long abandoned. It stays pending, and an operator's
    /// `reconcilePayment` still settles it.
    pub stop_after: Duration,
    /// The most top-ups one pass claims.
    pub batch: u32,
}

impl Default for ReconcileSettings {
    /// 15 minutes before the first read, 15 minutes between reads, 7 days at most, 100 at a time.
    fn default() -> Self {
        Self {
            pending_after: Duration::from_secs(15 * 60),
            read_again_after: Duration::from_secs(15 * 60),
            stop_after: Duration::from_secs(7 * 24 * 3600),
            batch: 100,
        }
    }
}

/// The ledger's side of top-ups: an extension of the storage port, as [`PartitionRetention`](crate::PartitionRetention)
/// is. `openbilling-pg` implements it; its conformance suite is `openbilling-testkit`'s `top_up_conformance!`.
pub trait TopUpLedger: Send + Sync {
    /// Records a top-up. Once-only by the payment id: the same terms again answer `true` (a replay) and change
    /// nothing; other terms are refused as [`Rule::TopUpConflict`](crate::Rule::TopUpConflict), and an account that does
    /// not exist as `unknown_account`.
    fn record_top_up(
        &self,
        top_up: &TopUp,
    ) -> impl Future<Output = Result<bool, LedgerError>> + Send;

    /// Compares a payment read back with the top-up recorded for it and, when they agree and it succeeded, credits the
    /// *recorded* terms once. A difference marks the top-up as mismatched for good and credits nothing. What happens
    /// to a payment with no top-up is the deployment's [`UnrecordedTopUps`] policy.
    fn settle_top_up(
        &self,
        payment: &PaymentView,
    ) -> impl Future<Output = Result<TopUpSettled, LedgerError>> + Send;

    /// Claims up to `settings.batch` pending top-ups that are due to be read: older than `pending_after`, younger than
    /// `stop_after`, and not claimed in the last `read_again_after`. A claim is visible at once to every other
    /// caller, so two claims never answer the same top-up.
    fn claim_pending_top_ups(
        &self,
        settings: &ReconcileSettings,
    ) -> impl Future<Output = Result<Vec<CreditId>, LedgerError>> + Send;
}

/// What one reconciliation pass did, by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Top-ups credited now (or found credited already).
    pub credited: u32,
    /// Top-ups the provider still holds as unpaid.
    pub pending: u32,
    /// Top-ups the provider canceled.
    pub canceled: u32,
    /// Top-ups whose payment differs from what was recorded.
    pub mismatched: u32,
    /// Top-ups that could not be read or settled now; they are read again later.
    pub failed: u32,
}

impl ReconcileReport {
    /// How many top-ups the pass looked at.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.credited + self.pending + self.canceled + self.mismatched + self.failed
    }
}

/// One reconciliation pass: claims the pending top-ups that are due and reads each back from the provider. A top-up
/// that cannot be settled now is counted and left to a later pass; one failure never stops the others.
///
/// # Errors
///
/// [`LedgerError`] when the claim fails.
pub async fn reconcile_pending<L, P>(
    ledger: &L,
    provider: &P,
    settings: &ReconcileSettings,
) -> Result<ReconcileReport, LedgerError>
where
    L: TopUpLedger,
    P: PaymentProvider,
{
    let mut report = ReconcileReport::default();
    for id in ledger.claim_pending_top_ups(settings).await? {
        match settle_payment(ledger, provider, &id).await {
            Ok(Settled::Credited(_)) => report.credited += 1,
            Ok(Settled::PaymentNotSettled(PaymentStatus::Canceled)) => report.canceled += 1,
            Ok(_) => report.pending += 1,
            Err(SettleError::ReadBackMismatch(_)) => report.mismatched += 1,
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mismatch_field_has_a_stable_name_and_reads_back() {
        for field in MismatchField::ALL {
            assert_eq!(MismatchField::from_name(field.as_str()), Some(field));
        }
        assert_eq!(MismatchField::from_name("secret"), None);
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let s = ReconcileSettings::default();
        assert_eq!(s.pending_after, Duration::from_secs(900));
        assert_eq!(s.read_again_after, Duration::from_secs(900));
        assert_eq!(s.stop_after, Duration::from_secs(604_800));
        assert_eq!(s.batch, 100);
        assert_eq!(UnrecordedTopUps::default(), UnrecordedTopUps::Refuse);
    }
}
