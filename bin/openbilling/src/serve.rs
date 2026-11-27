//! `openbilling serve`: connect, migrate, then run the API, the drain and the partition job until a signal:
//! Ctrl-C (`SIGINT`) or `SIGTERM`, the signal a container runtime sends to stop a container.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use openbilling_core::{Classify, LedgerStore};
use openbilling_http::auth::{BearerAuth, RefuseAll};
use openbilling_http::{Services, prepare, router};
use openbilling_jwt::JwtVerifier;
use openbilling_pg::{LedgerSettings, PgLedger, PgStore, Woken};
use openbilling_vpay::VpayProvider;

use crate::config::Settings;
use crate::{ACCENT, WARN};

/// How many events one drain takes.
const DRAIN_BATCH: u32 = 500;
/// How long the drain waits for a notification before it polls anyway (ADR 0001, point 16).
const POLL: Duration = Duration::from_secs(5);
/// How often partitions are made ahead (ADR 0001, point 10).
const PARTITIONS_EVERY: Duration = Duration::from_secs(3600);

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
        match ledger.drain(DRAIN_BATCH).await {
            Ok(report) if !report.decided.is_empty() => continue,
            Ok(_) => {}
            Err(error) => {
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

async fn partitions_forever(ledger: PgLedger) {
    loop {
        tokio::time::sleep(PARTITIONS_EVERY).await;
        if let Err(error) = ledger.prepare_partitions().await {
            anstream::eprintln!(
                "{WARN}openbilling:{WARN:#} preparing partitions failed ({})",
                error.class()
            );
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

pub async fn serve(settings: Settings) -> Result<()> {
    // The URL is never in an error: openbilling-pg's errors are fixed text.
    let store = PgStore::connect(settings.database_url.expose())
        .await
        .context("connecting to PostgreSQL")?;
    store.migrate().await.context("applying the migrations")?;
    let ledger = PgLedger::new(&store, LedgerSettings::default());
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
    let allowed = settings.allowed;
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
    let partitions = tokio::spawn(partitions_forever(ledger));

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
    let _ = drain.await;
    let _ = partitions.await;
    store.close().await;
    Ok(())
}
