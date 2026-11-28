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
use crate::payment::{
    IdempotencyKey, PaymentProvider, PaymentRefusal, PaymentStatus, PaymentTarget, PaymentView,
};
use crate::refusal::LedgerError;
use crate::settle::{SettleError, Settled, settle_payment};
use crate::store::Credited;
use crate::{Amount, Classify, CurrencyCode, ErrorClass};

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
    /// A top-up this old is read back one last time ([`TopUpLedger::claim_expiring_top_ups`]); only one still
    /// pending at the provider becomes `expired` ([`TopUpLedger::expire_top_up`]), and a webhook or an operator's
    /// `reconcilePayment` still settles it.
    pub stop_after: Duration,
    /// How long after `stop_after` the backstop ([`TopUpLedger::expire_stale_top_ups`]) expires a top-up whose last
    /// read never came to an answer (the provider kept failing, or the process stopped between claim and settle).
    pub last_read_for: Duration,
    /// The most top-ups one pass claims.
    pub batch: u32,
}

impl Default for ReconcileSettings {
    /// 15 minutes before the first read, 15 minutes between reads, a last read at 7 days with a day's grace for it,
    /// 100 at a time.
    fn default() -> Self {
        Self {
            pending_after: Duration::from_secs(15 * 60),
            read_again_after: Duration::from_secs(15 * 60),
            stop_after: Duration::from_secs(7 * 24 * 3600),
            last_read_for: Duration::from_secs(24 * 3600),
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
    /// `stop_after`, and not claimed in the last `read_again_after`, the least recently read first, so a backlog
    /// larger than a batch is covered before any top-up is read twice. A claim is visible at once to every other
    /// caller, so two claims never answer the same top-up.
    fn claim_pending_top_ups(
        &self,
        settings: &ReconcileSettings,
    ) -> impl Future<Output = Result<Vec<CreditId>, LedgerError>> + Send;

    /// Claims up to `settings.batch` pending top-ups that reached `stop_after` and are due for their last read: older
    /// than `stop_after` and not claimed in the last `read_again_after`, the oldest first. Disjoint from
    /// [`Self::claim_pending_top_ups`]. A claim is visible at once to every other caller. A top-up claimed and not
    /// settled stays pending and is claimed again after `read_again_after`.
    fn claim_expiring_top_ups(
        &self,
        settings: &ReconcileSettings,
    ) -> impl Future<Output = Result<Vec<CreditId>, LedgerError>> + Send;

    /// Moves one top-up to `expired` when it is still pending and at least `settings.stop_after` old, and answers
    /// whether it did: `false` means it was settled meanwhile (a webhook, another pass) and nothing changed.
    fn expire_top_up(
        &self,
        payment: &CreditId,
        settings: &ReconcileSettings,
    ) -> impl Future<Output = Result<bool, LedgerError>> + Send;

    /// The backstop: moves the pending top-ups older than `settings.stop_after + settings.last_read_for` to
    /// `expired` without a read in this call, only those that had at least one last-read attempt (a claim by
    /// [`Self::claim_expiring_top_ups`], that is `last_read_at >= created_at + stop_after`), at most `settings.batch` of them, and answers how many. An expired top-up is never
    /// claimed, and a webhook or `reconcilePayment` still settles it.
    fn expire_stale_top_ups(
        &self,
        settings: &ReconcileSettings,
    ) -> impl Future<Output = Result<u64, LedgerError>> + Send;
}

/// What the reads of one kind (due, or last) did, by outcome. Each top-up is counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadOutcomes {
    /// Top-ups credited now.
    pub credited: u32,
    /// Top-ups found credited already (a replay: the webhook, or another pass, booked them first).
    pub replayed: u32,
    /// Top-ups the provider still holds as unpaid. For a last read these are the ones moved to `expired`.
    pub pending: u32,
    /// Payments that are not ours (no top-up and no owner reference): nothing was moved.
    pub not_ours: u32,
    /// Top-ups the provider canceled.
    pub canceled: u32,
    /// Top-ups whose payment differs from what was recorded.
    pub mismatched: u32,
    /// Top-ups that could not be read or settled now; they are read again later.
    pub failed: u32,
    /// Last reads only: top-ups settled by someone else between the read and the move to `expired`.
    pub settled_meanwhile: u32,
}

impl ReadOutcomes {
    /// How many top-ups were looked at.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.credited
            + self.replayed
            + self.pending
            + self.not_ours
            + self.canceled
            + self.mismatched
            + self.failed
            + self.settled_meanwhile
    }

    fn count(&mut self, result: &Result<Settled, SettleError>) {
        match result {
            Ok(Settled::Credited(credited)) if credited.replayed => self.replayed += 1,
            Ok(Settled::Credited(_)) => self.credited += 1,
            Ok(Settled::PaymentNotSettled(PaymentStatus::Canceled)) => self.canceled += 1,
            Ok(Settled::NotOurs) => self.not_ours += 1,
            Ok(_) => self.pending += 1,
            Err(SettleError::ReadBackMismatch(_)) => self.mismatched += 1,
            Err(_) => self.failed += 1,
        }
    }
}

/// What one reconciliation pass did, by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The last reads of the top-ups that reached `stop_after`: `last.pending` is how many a last read found still
    /// pending and moved to `expired`.
    pub last: ReadOutcomes,
    /// Top-ups whose last read was answered by the provider as "no such payment" and that were expired unread.
    pub expired_unread: u32,
    /// Top-ups credited now.
    pub credited: u32,
    /// Top-ups found credited already (a replay: the webhook, or another pass, booked them first).
    pub replayed: u32,
    /// Top-ups the provider still holds as unpaid.
    pub pending: u32,
    /// Payments that are not ours (no top-up and no owner reference): nothing was moved.
    pub not_ours: u32,
    /// Top-ups the provider canceled.
    pub canceled: u32,
    /// Top-ups whose payment differs from what was recorded.
    pub mismatched: u32,
    /// Top-ups that could not be read or settled now; they are read again later.
    pub failed: u32,
    /// Pending top-ups the backstop moved to `expired` without a read (not part of [`Self::total`]).
    pub expired: u64,
    /// The class of the error that stopped this pass's expiry, if one did; the claim still ran.
    pub expire_error: Option<ErrorClass>,
}

impl ReconcileReport {
    /// How many top-ups the pass looked at, due reads and last reads.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.last.total()
            + self.expired_unread
            + self.credited
            + self.replayed
            + self.pending
            + self.not_ours
            + self.canceled
            + self.mismatched
            + self.failed
    }
}

/// One reconciliation pass. First the top-ups that reached `stop_after` are claimed and read back one last time: one
/// that is paid is credited, one the provider canceled or that differs is closed, and only one still pending at the
/// provider becomes `expired`. A last read that fails in any way but one (the provider's explicit "no such payment",
/// [`PaymentRefusal::NotFound`], which expires it unread) leaves the top-up pending, counted as failed, and it is
/// read again after `read_again_after`; a ledger error never expires one. Then the backstop expires what stayed
/// pending past `stop_after + last_read_for` and had at least one last-read attempt. Last, the top-ups that are due
/// are claimed and read back. A top-up that cannot be settled now is counted and left
/// to a later pass; one failure never stops the others. One pass reads at most `2 * batch` payments.
///
/// # Errors
///
/// [`LedgerError`] when the claim fails. A failed expiry is reported in [`ReconcileReport::expire_error`] and the claim still runs.
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
    // The last read comes first, so a top-up past `stop_after` is read before the backstop can see it and is never
    // left unread because the batch was spent on younger ones.
    for id in ledger.claim_expiring_top_ups(settings).await? {
        let result = settle_payment(ledger, provider, &id).await;
        match &result {
            Ok(
                Settled::Credited(_)
                | Settled::NotOurs
                | Settled::PaymentNotSettled(PaymentStatus::Canceled),
            )
            | Err(SettleError::ReadBackMismatch(_)) => report.last.count(&result),
            Ok(_) => match ledger.expire_top_up(&id, settings).await {
                Ok(true) => report.last.pending += 1,
                Ok(false) => report.last.settled_meanwhile += 1,
                Err(_) => report.last.failed += 1,
            },
            // Only the provider's own answer that the payment does not exist ends a top-up unread. Anything else
            // (it is down, it failed, the ledger refused) leaves it pending, to be read again after
            // `read_again_after`: a paid top-up must never expire because a read failed.
            Err(SettleError::Provider(error))
                if error.refusal() == Some(PaymentRefusal::NotFound) =>
            {
                match ledger.expire_top_up(&id, settings).await {
                    Ok(true) => report.expired_unread += 1,
                    Ok(false) => report.last.settled_meanwhile += 1,
                    Err(_) => report.last.failed += 1,
                }
            }
            Err(_) => report.last.failed += 1,
        }
    }
    // Bounded steps, so a large backlog is worked off over passes and one pass never holds many rows.
    // A failed expiry is reported and never stops the claim: the due top-ups are still read back.
    for _ in 0..10 {
        match ledger.expire_stale_top_ups(settings).await {
            Ok(n) => {
                report.expired += n;
                if n < u64::from(settings.batch.max(1)) {
                    break;
                }
            }
            Err(error) => {
                report.expire_error = Some(error.class());
                break;
            }
        }
    }
    for id in ledger.claim_pending_top_ups(settings).await? {
        let result = settle_payment(ledger, provider, &id).await;
        match &result {
            Ok(Settled::Credited(credited)) if credited.replayed => report.replayed += 1,
            Ok(Settled::Credited(_)) => report.credited += 1,
            Ok(Settled::PaymentNotSettled(PaymentStatus::Canceled)) => report.canceled += 1,
            Ok(Settled::NotOurs) => report.not_ours += 1,
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
        assert_eq!(s.last_read_for, Duration::from_secs(86_400));
        assert_eq!(s.batch, 100);
        assert_eq!(UnrecordedTopUps::default(), UnrecordedTopUps::Refuse);
    }

    /// The last-read policy, driven by scripted fakes: no clock and no runtime, so nothing here depends on timing.
    mod last_read {
        use std::collections::VecDeque;
        use std::pin::pin;
        use std::sync::Mutex;
        use std::task::{Context, Poll, Waker};

        use super::super::*;
        use crate::Timestamp;
        use crate::payment::{PaymentError, PaymentRefusal, WebhookHeaders, WebhookHint};
        use crate::refusal::{Refusal, Rule};

        fn block_on<T>(future: impl Future<Output = T>) -> T {
            let mut cx = Context::from_waker(Waker::noop());
            let mut future = pin!(future);
            loop {
                if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                    return value;
                }
            }
        }

        fn id() -> CreditId {
            CreditId::new("pay-1").unwrap()
        }

        fn target() -> PaymentTarget {
            PaymentTarget {
                owner: crate::OwnerId::new("o1").unwrap(),
                label: crate::AccountLabel::new("main").unwrap(),
            }
        }

        fn view() -> PaymentView {
            PaymentView {
                id: id(),
                status: PaymentStatus::Succeeded,
                amount: Amount::from_micros(5).unwrap(),
                currency: CurrencyCode::new("EUR").unwrap(),
                to: Some(target()),
            }
        }

        fn credited() -> TopUpSettled {
            TopUpSettled::Credited(Credited {
                account: crate::AccountId::new("a1").unwrap(),
                transaction: "t".to_owned(),
                replayed: false,
                repaid: Amount::from_micros(0).unwrap(),
                debt_added: Amount::from_micros(0).unwrap(),
            })
        }

        fn io_error(class: ErrorClass) -> PaymentError {
            PaymentError::Provider {
                class,
                source: Box::new(std::io::Error::other("boom")),
            }
        }

        /// A provider that answers the scripted reads in order.
        struct Provider(Mutex<VecDeque<Result<PaymentView, PaymentError>>>);

        impl PaymentProvider for Provider {
            fn name(&self) -> &'static str {
                "fake"
            }
            async fn create_payment(
                &self,
                _: &crate::PaymentRequest,
            ) -> Result<crate::CreatedPayment, PaymentError> {
                unreachable!()
            }
            async fn read_payment(&self, _: &CreditId) -> Result<PaymentView, PaymentError> {
                self.0.lock().unwrap().pop_front().expect("a scripted read")
            }
            async fn read_refund(&self, _: &CreditId) -> Result<crate::RefundView, PaymentError> {
                unreachable!()
            }
            fn verify_webhook(
                &self,
                _: &WebhookHeaders,
                _: &[u8],
                _: Timestamp,
            ) -> Result<WebhookHint, PaymentError> {
                unreachable!()
            }
        }

        /// A ledger with one pending top-up, claimed for its last read on every pass; it says what was done to it.
        #[derive(Default)]
        struct Ledger {
            settles: Mutex<VecDeque<Result<TopUpSettled, LedgerError>>>,
            expired: Mutex<u32>,
        }

        impl TopUpLedger for Ledger {
            async fn record_top_up(&self, _: &TopUp) -> Result<bool, LedgerError> {
                unreachable!()
            }
            async fn settle_top_up(&self, _: &PaymentView) -> Result<TopUpSettled, LedgerError> {
                self.settles
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("a scripted settle")
            }
            async fn claim_pending_top_ups(
                &self,
                _: &ReconcileSettings,
            ) -> Result<Vec<CreditId>, LedgerError> {
                Ok(Vec::new())
            }
            async fn claim_expiring_top_ups(
                &self,
                _: &ReconcileSettings,
            ) -> Result<Vec<CreditId>, LedgerError> {
                Ok(vec![id()])
            }
            async fn expire_top_up(
                &self,
                _: &CreditId,
                _: &ReconcileSettings,
            ) -> Result<bool, LedgerError> {
                *self.expired.lock().unwrap() += 1;
                Ok(true)
            }
            async fn expire_stale_top_ups(
                &self,
                _: &ReconcileSettings,
            ) -> Result<u64, LedgerError> {
                Ok(0)
            }
        }

        fn pass(ledger: &Ledger, reads: Vec<Result<PaymentView, PaymentError>>) -> ReconcileReport {
            let provider = Provider(Mutex::new(reads.into()));
            block_on(reconcile_pending(
                ledger,
                &provider,
                &ReconcileSettings::default(),
            ))
            .unwrap()
        }

        #[test]
        fn a_provider_error_at_the_last_read_leaves_it_pending_and_a_later_read_credits_it() {
            for class in [
                ErrorClass::Internal,
                ErrorClass::Invalid,
                ErrorClass::Unavailable,
            ] {
                let ledger = Ledger::default();
                ledger.settles.lock().unwrap().push_back(Ok(credited()));
                let first = pass(&ledger, vec![Err(io_error(class))]);
                assert_eq!((first.last.failed, first.expired_unread), (1, 0), "{class}");
                assert_eq!(*ledger.expired.lock().unwrap(), 0, "{class}: never expired");
                // The next claim reads it again and credits it.
                let second = pass(&ledger, vec![Ok(view())]);
                assert_eq!(second.last.credited, 1, "{class}: {second:?}");
            }
            // A refusal of the provider other than not-found is no answer either.
            let ledger = Ledger::default();
            let report = pass(&ledger, vec![Err(PaymentRefusal::Conflict.into())]);
            assert_eq!((report.last.failed, report.expired_unread), (1, 0));
            assert_eq!(*ledger.expired.lock().unwrap(), 0);
        }

        #[test]
        fn a_ledger_error_after_a_successful_read_leaves_it_pending() {
            let ledger = Ledger::default();
            ledger
                .settles
                .lock()
                .unwrap()
                .push_back(Err(LedgerError::Refused(Refusal::new(
                    Rule::UnknownAccount,
                ))));
            let report = pass(&ledger, vec![Ok(view())]);
            assert_eq!((report.last.failed, report.expired_unread), (1, 0));
            assert_eq!(
                *ledger.expired.lock().unwrap(),
                0,
                "never expired on a ledger error"
            );
        }

        #[test]
        fn only_a_not_found_expires_it_unread() {
            let ledger = Ledger::default();
            let report = pass(&ledger, vec![Err(PaymentRefusal::NotFound.into())]);
            assert_eq!((report.expired_unread, report.last.failed), (1, 0));
            assert_eq!(*ledger.expired.lock().unwrap(), 1);
        }
    }
}
