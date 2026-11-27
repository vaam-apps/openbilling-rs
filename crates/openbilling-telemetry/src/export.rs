//! The exporter: batches finished spans and logs, ships metrics on an interval, and posts each as an OTLP/HTTP
//! JSON document. It never blocks the code that records: spans and logs reach it through a bounded queue with
//! `try_send`, and a full queue drops (and counts). A failed export is retried a few times and then dropped: the
//! telemetry is never the reason a request waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as Json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing_subscriber::layer::SubscriberExt;
use url::Url;

use crate::layer::TelemetryLayer;
use crate::metrics::Metrics;
use crate::otlp::{LogRecord, SpanRecord, logs_request, metrics_request, traces_request};
use crate::settings::{Resource, Settings};

const QUEUE: usize = 4096;
const BATCH: usize = 512;
const ATTEMPTS: u32 = 3;

/// Why telemetry did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// A header of `OPENBILLING_OTEL_HEADERS_FILE` is not a valid HTTP header.
    #[error("OPENBILLING_OTEL_HEADERS_FILE holds a header that is not valid")]
    Header,
    /// The HTTP client could not be built.
    #[error("the OTLP client could not be built")]
    Client,
    /// A `tracing` subscriber is already installed in this process.
    #[error("a tracing subscriber is already installed")]
    Subscriber,
}

/// What the exporter did, in counts.
#[derive(Debug, Default)]
pub struct Stats {
    /// Requests the collector accepted.
    pub exported: AtomicU64,
    /// Requests that failed after every attempt.
    pub failed: AtomicU64,
}

#[derive(Clone)]
struct Sender {
    client: reqwest::Client,
    headers: HeaderMap,
    timeout: Duration,
    stats: Arc<Stats>,
    last_warning: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl Sender {
    fn new(settings: &Settings, stats: Arc<Stats>) -> Result<Self, StartError> {
        // reqwest is built with no crypto provider of its own: rustls uses the process default, ring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (name, value) in &settings.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| StartError::Header)?;
            let mut value = HeaderValue::from_str(value).map_err(|_| StartError::Header)?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| StartError::Client)?;
        Ok(Self {
            client,
            headers,
            timeout: settings.timeout,
            stats,
            last_warning: Arc::default(),
        })
    }

    fn warn(&self, what: &str) {
        let mut last = self
            .last_warning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            *last = Some(Instant::now());
            // Stderr, not `tracing`: the exporter must not feed itself. No URL, no header, no body.
            eprintln!("openbilling: telemetry export failed ({what}); dropping, will keep trying");
        }
    }

    async fn post(&self, url: &Url, document: &Json) {
        let Ok(body) = serde_json::to_vec(document) else {
            return;
        };
        let mut what = "no answer";
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
            let sent = self
                .client
                .post(url.clone())
                .headers(self.headers.clone())
                .timeout(self.timeout)
                .body(body.clone())
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => {
                    self.stats.exported.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !matches!(status, 429 | 502 | 503 | 504) {
                        what = "refused by the collector";
                        break;
                    }
                    what = "the collector is busy";
                }
                Err(_) => what = "network error",
            }
        }
        self.stats.failed.fetch_add(1, Ordering::Relaxed);
        self.warn(what);
    }
}

/// Collects items until a batch is full or `delay` passes, and hands each batch to `send`; on `stop`, flushes
/// what is queued and ends.
async fn batches<T, F, Fut>(
    mut rx: mpsc::Receiver<T>,
    mut stop: watch::Receiver<bool>,
    delay: Duration,
    send: F,
) where
    F: Fn(Vec<T>) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut batch = Vec::new();
    let mut tick = tokio::time::interval(delay);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            item = rx.recv() => match item {
                Some(item) => {
                    batch.push(item);
                    if batch.len() >= BATCH {
                        send(std::mem::take(&mut batch)).await;
                    }
                }
                None => break,
            },
            _ = tick.tick() => {
                if !batch.is_empty() {
                    send(std::mem::take(&mut batch)).await;
                }
            }
            _ = stop.changed() => break,
        }
    }
    while let Ok(item) = rx.try_recv() {
        batch.push(item);
    }
    if !batch.is_empty() {
        send(batch).await;
    }
}

async fn metrics_loop(
    metrics: &'static Metrics,
    sender: Sender,
    url: Url,
    resource: Resource,
    mut stop: watch::Receiver<bool>,
    every: Duration,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        let last = tokio::select! {
            _ = tick.tick() => false,
            _ = stop.changed() => true,
        };
        let document = metrics_request(&resource, &metrics.snapshot(), SystemTime::now());
        sender.post(&url, &document).await;
        if last {
            break;
        }
    }
}

/// The running exporter. Dropping it does not stop it: call [`Guard::shutdown`] to flush before the process ends.
pub struct Guard {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    stats: Arc<Stats>,
    layer: TelemetryLayer,
}

impl Guard {
    /// What the exporter did.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Spans and logs dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.layer.dropped()
    }

    /// Flushes everything queued and the metrics one last time, waiting at most `deadline`.
    pub async fn shutdown(self, deadline: Duration) {
        let _ = self.stop.send(true);
        let wait = async {
            for task in self.tasks {
                let _ = task.await;
            }
        };
        let _ = tokio::time::timeout(deadline, wait).await;
    }
}

/// Starts exporting: installs the `tracing` subscriber of this process and spawns the exporter's tasks on the
/// current tokio runtime. Call it once, inside the runtime.
///
/// # Errors
///
/// [`StartError`] for an invalid header, a client that cannot be built, or a subscriber already installed.
pub fn start(settings: &Settings, metrics: &'static Metrics) -> Result<Guard, StartError> {
    let stats = Arc::new(Stats::default());
    let sender = Sender::new(settings, Arc::clone(&stats))?;
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    let span_tx = settings.signals.traces.then(|| {
        let (tx, rx) = mpsc::channel::<SpanRecord>(QUEUE);
        let (sender, url, resource) = (
            sender.clone(),
            settings.traces_url.clone(),
            settings.resource.clone(),
        );
        tasks.push(tokio::spawn(batches(
            rx,
            stop_rx.clone(),
            settings.batch_delay,
            move |spans: Vec<SpanRecord>| {
                let (sender, url, document) = (
                    sender.clone(),
                    url.clone(),
                    traces_request(&resource, &spans),
                );
                async move { sender.post(&url, &document).await }
            },
        )));
        tx
    });
    let log_tx = settings.signals.logs.then(|| {
        let (tx, rx) = mpsc::channel::<LogRecord>(QUEUE);
        let (sender, url, resource) = (
            sender.clone(),
            settings.logs_url.clone(),
            settings.resource.clone(),
        );
        tasks.push(tokio::spawn(batches(
            rx,
            stop_rx.clone(),
            settings.batch_delay,
            move |logs: Vec<LogRecord>| {
                let (sender, url, document) =
                    (sender.clone(), url.clone(), logs_request(&resource, &logs));
                async move { sender.post(&url, &document).await }
            },
        )));
        tx
    });
    if settings.signals.metrics {
        tasks.push(tokio::spawn(metrics_loop(
            metrics,
            sender,
            settings.metrics_url.clone(),
            settings.resource.clone(),
            stop_rx,
            settings.metric_interval,
        )));
    }

    let layer = TelemetryLayer::new(span_tx, log_tx, settings.sample_ratio, metrics);
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer.clone()))
        .map_err(|_| StartError::Subscriber)?;
    Ok(Guard {
        stop,
        tasks,
        stats,
        layer,
    })
}
