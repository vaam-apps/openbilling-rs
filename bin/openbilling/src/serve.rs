//! `openbilling serve`: connect, migrate, then run the API, the drain, the partition job and, with a payment provider,
//! the top-up reconciler until a signal:
//! Ctrl-C (`SIGINT`) or `SIGTERM`, the signal a container runtime sends to stop a container.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use openbilling_core::{Classify, ErrorClass, LedgerStore, ReconcileSettings, reconcile_pending};
use openbilling_http::auth::{BearerAuth, RefuseAll};
use openbilling_http::{Services, prepare, router};
use openbilling_jwt::JwtVerifier;
use openbilling_pg::{LedgerSettings, PgLedger, PgStore, Woken};
use openbilling_telemetry::{Counter, global};
use openbilling_vpay::VpayProvider;
use tracing::Instrument as _;

use crate::config::Settings;
use crate::{ACCENT, WARN};

/// How many events one drain takes.
const DRAIN_BATCH: u32 = 500;
/// How long the drain waits for a notification before it polls anyway (ADR 0001, point 16).
const POLL: Duration = Duration::from_secs(5);
/// How often partitions are made ahead (ADR 0001, point 10).
const PARTITIONS_EVERY: Duration = Duration::from_secs(3600);
/// How often the queue and pool gauges are read.
const SAMPLE_EVERY: Duration = Duration::from_secs(15);
/// How long the exporter may take to flush when the service stops.
const FLUSH: Duration = Duration::from_secs(5);

/// Counts an error of a loop by class and where it happened.
fn count_error(class: openbilling_core::ErrorClass, source: &'static str) {
    global().add(
        Counter::Errors,
        &[("class", class.as_str()), ("source", source)],
        1,
    );
}

/// Drains until a drain takes nothing, then waits for a wake-up or the poll. A failure is reported by its class
/// and retried after the poll: the table is the truth.
async fn drain_forever(ledger: PgLedger) {
    let mut wakeup = match ledger.wakeup().await {
        Ok(wakeup) => Some(wakeup),
        Err(error) => {
            anstream::eprintln!(
                "{WARN}openbilling:{WARN:#} no LISTEN connection ({}); polling only",
                error.class()
            );
            None
        }
    };
    loop {
        // One span per pass: the database calls it makes are its children.
        let pass = tracing::info_span!(
            "drain.cycle",
            openbilling.batch.limit = u64::from(DRAIN_BATCH),
            openbilling.events.decided = tracing::field::Empty,
            openbilling.events.applied = tracing::field::Empty,
            openbilling.events.refused = tracing::field::Empty,
        );
        match ledger.drain(DRAIN_BATCH).instrument(pass.clone()).await {
            Ok(report) if !report.decided.is_empty() => {
                pass.record("openbilling.events.decided", report.decided.len() as u64);
                pass.record("openbilling.events.applied", report.applied() as u64);
                pass.record(
                    "openbilling.events.refused",
                    (report.decided.len() - report.applied()) as u64,
                );
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                count_error(error.class(), "drain");
                tracing::warn!(
                    openbilling.error.class = error.class().as_str(),
                    "drain failed"
                );
                anstream::eprintln!(
                    "{WARN}openbilling:{WARN:#} drain failed ({})",
                    error.class()
                );
            }
        }
        match wakeup.as_mut() {
            Some(wakeup) => match wakeup.wait(POLL).await {
                Ok(Woken::Notified | Woken::Polled) => {}
                Err(_) => tokio::time::sleep(POLL).await,
            },
            None => tokio::time::sleep(POLL).await,
        }
    }
}

/// Makes partitions ahead, then drops the expired ones (ADR 0001, point 10), every hour. Dropping is leader-safe:
/// the store's job takes an advisory lock, so on several instances one runs and the others do nothing. Only counts
/// are logged.
async fn partitions_forever(ledger: PgLedger) {
    // The first pass runs at once, in the background: a detach waits for every transaction on the table, and the
    // service must not wait for it to listen (a rolling restart would never become ready).
    let mut first = true;
    loop {
        if !first {
            tokio::time::sleep(PARTITIONS_EVERY).await;
        }
        first = false;
        let pass = tracing::info_span!("partitions.run");
        if let Err(error) = ledger.prepare_partitions().instrument(pass).await {
            count_error(error.class(), "partitions");
            tracing::warn!(
                openbilling.error.class = error.class().as_str(),
                "preparing partitions failed"
            );
            anstream::eprintln!(
                "{WARN}openbilling:{WARN:#} preparing partitions failed ({})",
                error.class()
            );
        }
        let pass = tracing::info_span!("retention.run");
        match ledger.run_retention().instrument(pass).await {
            Ok(report) => {
                if !report.dropped.is_empty() {
                    anstream::eprintln!(
                        "{ACCENT}openbilling:{ACCENT:#} dropped {} expired usage partition(s)",
                        report.dropped.len()
                    );
                }
            }
            Err(error) => {
                count_error(error.class(), "retention");
                tracing::warn!(
                    openbilling.error.class = error.class().as_str(),
                    "dropping expired partitions failed"
                );
                anstream::eprintln!(
                    "{WARN}openbilling:{WARN:#} dropping expired partitions failed ({})",
                    error.class()
                );
            }
        }
    }
}

/// Reads back the pending top-ups that are due, every `every` (ADR 0003, amended 2026-11-28): a paid top-up whose webhook
/// never came is found here. Safe on any number of instances: a claim takes each top-up for one caller. A failure of
/// a pass is counted and logged by class and retried at the next one; a top-up that could not be settled is read again
/// later. Only counts are logged.
async fn reconcile_forever(
    ledger: PgLedger,
    provider: VpayProvider,
    settings: ReconcileSettings,
    every: Duration,
) {
    loop {
        tokio::time::sleep(every).await;
        let pass = tracing::info_span!("reconcile.run");
        // Each pass is its own task: a panic in one (a bug, never a normal failure) is caught at the join, counted as
        // an error, and the next pass runs, so reconciliation does not silently end.
        let run = tokio::spawn({
            let (ledger, provider, settings) = (ledger.clone(), provider.clone(), settings);
            async move {
                reconcile_pending(&ledger, &provider, &settings)
                    .instrument(pass)
                    .await
            }
        });
        let result = match run.await {
            Ok(result) => result,
            Err(joined) => {
                global().add(Counter::TopUpsReconciled, &[("outcome", "error")], 1);
                count_error(ErrorClass::Internal, "reconcile");
                tracing::error!(
                    openbilling.error.class = "internal",
                    panicked = joined.is_panic(),
                    "a reconciliation pass did not finish"
                );
                anstream::eprintln!(
                    "{WARN}openbilling:{WARN:#} a reconciliation pass did not finish; the next one runs on time"
                );
                continue;
            }
        };
        match result {
            Ok(report) => {
                for (outcome, n) in [
                    ("credited", report.credited),
                    ("replayed", report.replayed),
                    ("pending", report.pending),
                    ("not_ours", report.not_ours),
                    ("canceled", report.canceled),
                    ("mismatch", report.mismatched),
                    ("error", report.failed),
                ] {
                    if n > 0 {
                        global().add(
                            Counter::TopUpsReconciled,
                            &[("outcome", outcome)],
                            u64::from(n),
                        );
                    }
                }
                if report.credited + report.mismatched + report.failed > 0 {
                    anstream::eprintln!(
                        "{ACCENT}openbilling:{ACCENT:#} read back {} pending top-up(s): {} credited, {} differed, {} failed",
                        report.total(),
                        report.credited,
                        report.mismatched,
                        report.failed
                    );
                }
            }
            Err(error) => {
                count_error(error.class(), "reconcile");
                tracing::warn!(
                    openbilling.error.class = error.class().as_str(),
                    "reading back pending top-ups failed"
                );
                anstream::eprintln!(
                    "{WARN}openbilling:{WARN:#} reading back pending top-ups failed ({})",
                    error.class()
                );
            }
        }
    }
}

/// The stop signals, registered before `serve` says it is listening: a signal that arrives after that line must
/// find its handler installed, or the default action would end the process without a clean stop.
struct StopSignals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl StopSignals {
    fn register() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let terminate = match signal(SignalKind::terminate()) {
                Ok(terminate) => Some(terminate),
                Err(_) => {
                    anstream::eprintln!(
                        "{WARN}openbilling:{WARN:#} no SIGTERM handler; stopping on Ctrl-C only"
                    );
                    None
                }
            };
            Self { terminate }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Resolves on Ctrl-C (`SIGINT`) or, on Unix, `SIGTERM`. Kubernetes stops a pod with `SIGTERM`, and a process
    /// that is PID 1 in its container ignores a signal it has no handler for: without this, a pod would wait out its
    /// grace period and be killed, never draining its connections.
    async fn wait(self) {
        #[cfg(unix)]
        if let Some(mut terminate) = self.terminate {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Reads the queue and pool gauges now and then.
async fn sample_forever(ledger: PgLedger) {
    loop {
        ledger.sample_metrics().await;
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
}

pub async fn serve(settings: Settings) -> Result<()> {
    let telemetry = start_telemetry(&settings)?;
    let result = run(settings).await;
    // On every way out, a refusal at start included: what was queued is flushed, and the metrics go out once more.
    if let Some(telemetry) = telemetry {
        telemetry.shutdown(FLUSH).await;
    }
    result
}

fn start_telemetry(settings: &Settings) -> Result<Option<openbilling_telemetry::Guard>> {
    // Telemetry first (the caller), so the start-up work is traced too. Off unless an endpoint is set; the metrics
    // registry records either way and costs a lock and an add.
    let Some(otel) = &settings.otel else {
        return Ok(None);
    };
    let guard = openbilling_telemetry::start(otel, global())
        .context("starting the OpenTelemetry exporter")?;
    anstream::eprintln!(
        "{ACCENT}openbilling:{ACCENT:#} exporting OpenTelemetry (traces {}, metrics {}, logs {})",
        otel.signals.traces,
        otel.signals.metrics,
        otel.signals.logs
    );
    Ok(Some(guard))
}

async fn run(settings: Settings) -> Result<()> {
    // The URL is never in an error: openbilling-pg's errors are fixed text.
    let store = PgStore::connect(settings.database_url.expose())
        .await
        .context("connecting to PostgreSQL")?;
    store.migrate().await.context("applying the migrations")?;
    let ledger = PgLedger::new(
        &store,
        LedgerSettings {
            retention_margin: settings.retention_margin,
            retention_timeout: settings.retention_timeout,
            unrecorded_top_ups: settings.unrecorded_top_ups,
            ..LedgerSettings::default()
        },
    );
    ledger
        .prepare_partitions()
        .await
        .context("preparing partitions")?;
    prepare(store.pool())
        .await
        .map_err(|_| anyhow::anyhow!("creating CrateStack's idempotency table failed"))?;
    let provider = settings
        .vpay
        .map(VpayProvider::new)
        .transpose()
        .context("the vpay configuration")?;
    if provider.is_none() {
        anstream::eprintln!(
            "{WARN}openbilling:{WARN:#} no payment provider configured; top-ups and webhooks answer 503"
        );
    }
    let reconciler = provider.clone().map(|provider| {
        anstream::eprintln!(
            "{ACCENT}openbilling:{ACCENT:#} pending top-ups are read back every {}s once {}s old, until {}s old; a succeeded payment with no recorded top-up is {}",
            settings.reconcile_every.as_secs(),
            settings.reconcile.pending_after.as_secs(),
            settings.reconcile.stop_after.as_secs(),
            settings.unrecorded_top_ups.as_str()
        );
        (provider, settings.reconcile, settings.reconcile_every)
    });
    let allowed = settings.allowed;
    anstream::eprintln!(
        "{ACCENT}openbilling:{ACCENT:#} usage partitions are dropped {} days after the window",
        settings.retention_margin.days()
    );
    anstream::eprintln!(
        "{ACCENT}openbilling:{ACCENT:#} getAllowed refuses on debt_limit {}, balance {}, member_cap {}",
        allowed.debt_limit.as_str(),
        allowed.balance.as_str(),
        allowed.member_cap.as_str()
    );
    let services = Arc::new(Services {
        ledger: ledger.clone(),
        provider,
        allowed,
    });
    let app = match settings.auth {
        Some(config) => {
            let verifier =
                JwtVerifier::new(config).context("the token verifier's configuration")?;
            anstream::eprintln!(
                "{ACCENT}openbilling:{ACCENT:#} /v1 accepts the bearer tokens of {} for the audience {}",
                verifier.config().issuer,
                verifier.config().audience
            );
            router(services, store.pool().clone(), BearerAuth::new(verifier))
        }
        None => {
            anstream::eprintln!(
                "{WARN}openbilling:{WARN:#} no token issuer configured; every /v1 call answers 401 \
                 (set OPENBILLING_AUTH_ISSUER and OPENBILLING_AUTH_AUDIENCE)"
            );
            router(services, store.pool().clone(), BearerAuth::new(RefuseAll))
        }
    };

    let stop = StopSignals::register();
    let drain = tokio::spawn(drain_forever(ledger.clone()));
    let partitions = tokio::spawn(partitions_forever(ledger.clone()));
    let reconcile = reconciler.map(|(provider, reconcile, every)| {
        tokio::spawn(reconcile_forever(
            ledger.clone(),
            provider,
            reconcile,
            every,
        ))
    });
    let sampler = tokio::spawn(sample_forever(ledger));

    let listener = tokio::net::TcpListener::bind(settings.listen)
        .await
        .with_context(|| format!("listening on {}", settings.listen))?;
    anstream::eprintln!(
        "{ACCENT}openbilling:{ACCENT:#} listening on {}",
        settings.listen
    );
    cratestack::axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(stop.wait())
    .await
    .context("serving")?;
    // The drain holds a LISTEN connection for as long as it runs, and closing the pool waits for every connection
    // to come back: stop both loops first. A drain cut short rolls its transaction back; the table is the truth.
    drain.abort();
    partitions.abort();
    sampler.abort();
    if let Some(reconcile) = &reconcile {
        reconcile.abort();
    }
    let _ = drain.await;
    let _ = partitions.await;
    let _ = sampler.await;
    if let Some(reconcile) = reconcile {
        let _ = reconcile.await;
    }
    store.close().await;
    Ok(())
}
