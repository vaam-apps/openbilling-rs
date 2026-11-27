//! The service's metrics: a small registry of closed instruments, with no dependency, so the crates that record
//! (`openbilling-pg`, `openbilling-http`) do not depend on an HTTP client. `openbilling-telemetry` exports it.
//!
//! **No instrument can carry an identity or an amount.** A counter adds a count; a histogram records a duration; a
//! gauge holds a count or an age. A label is a pair of `&'static str`: a name and a value that is a literal or a
//! closed enum's `as_str` in the code that records it, so a person's id, a token or an amount cannot be one. The
//! registry is process-wide ([`global`]) and cheap when nothing is exported: recording is a lock and an add.
//!
//! Counters and histograms are cumulative (OTLP aggregation temporality 2). Gauges are set by a sampler
//! ([`Metrics::set`]). The drain's lag is computed when the metrics are read.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Instant, SystemTime};

/// One label: a name and a value, both static.
pub type Pair = (&'static str, &'static str);

/// The labels of one recording.
pub type Labels<'a> = &'a [Pair];

/// The monotonic counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Counter {
    /// Events at intake: `result` is `accepted`, `replayed` or `refused`; `reason` is the rule of a refusal.
    IntakeEvents,
    /// Events a drain decided: `outcome` is `applied` or `refused`; `reason` is the refusal's.
    DrainEvents,
    /// Drain passes: `result` is `ok` or `error`.
    DrainRuns,
    /// Ledger postings, one per transaction booked, replays not counted: `kind` is `usage_debit`, `credit`,
    /// `allowance` or `reversal`.
    LedgerPostings,
    /// `getAllowed` verdicts: `check` is the check that decided (`debt_limit`, `balance`, `member_cap`, or `none`
    /// when allowed), `mode` the deployer's comparison it ran under (`at_or_above`, `above`, `never`) and `verdict`
    /// is `allowed` or `refused`.
    AllowedChecks,
    /// Errors by class: `class` is `invalid`, `conflict`, `unavailable` or `internal`; `source` is where.
    Errors,
    /// Spans or logs dropped because the queue to the exporter was full: `signal` is `traces` or `logs`.
    TelemetryDropped,
    /// Export requests to the collector: `signal` and `result` (`ok` or `failed`).
    TelemetryExports,
}

impl Counter {
    const ALL: [Self; 8] = [
        Self::IntakeEvents,
        Self::DrainEvents,
        Self::DrainRuns,
        Self::LedgerPostings,
        Self::AllowedChecks,
        Self::Errors,
        Self::TelemetryDropped,
        Self::TelemetryExports,
    ];

    /// The instrument's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::IntakeEvents => "openbilling.intake.events",
            Self::DrainEvents => "openbilling.drain.events",
            Self::DrainRuns => "openbilling.drain.runs",
            Self::LedgerPostings => "openbilling.ledger.postings",
            Self::AllowedChecks => "openbilling.allowed.checks",
            Self::Errors => "openbilling.errors",
            Self::TelemetryDropped => "openbilling.telemetry.dropped",
            Self::TelemetryExports => "openbilling.telemetry.exports",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::IntakeEvents => {
                "Usage events at intake, by result and the rule that refused them"
            }
            Self::DrainEvents => "Usage events decided by the drain, by outcome and refusal reason",
            Self::DrainRuns => "Drain passes, by result",
            Self::LedgerPostings => "Ledger transactions booked, by kind",
            Self::AllowedChecks => "getAllowed verdicts, by the check that decided",
            Self::Errors => "Errors by class and where they happened",
            Self::TelemetryDropped => {
                "Spans and logs dropped because the exporter's queue was full"
            }
            Self::TelemetryExports => "Export requests to the collector, by signal and result",
        }
    }

    const fn unit(self) -> &'static str {
        match self {
            Self::IntakeEvents | Self::DrainEvents => "{event}",
            Self::DrainRuns => "{run}",
            Self::LedgerPostings => "{posting}",
            Self::AllowedChecks => "{check}",
            Self::Errors => "{error}",
            Self::TelemetryDropped => "{item}",
            Self::TelemetryExports => "{request}",
        }
    }
}

/// The duration histograms, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Histogram {
    /// An HTTP request, by `method`, `route` and `status` class: `http.server.request.duration`.
    HttpServer,
    /// One drain pass.
    Drain,
    /// One database operation, by `operation`.
    Db,
}

impl Histogram {
    const ALL: [Self; 3] = [Self::HttpServer, Self::Drain, Self::Db];

    /// The instrument's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::HttpServer => "http.server.request.duration",
            Self::Drain => "openbilling.drain.duration",
            Self::Db => "db.client.operation.duration",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::HttpServer => "Duration of HTTP requests served",
            Self::Drain => "Duration of one drain pass",
            Self::Db => "Duration of one database operation of the ledger",
        }
    }
}

/// The gauges a sampler sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gauge {
    /// Events received and not yet decided.
    QueueBacklog,
    /// Age in seconds of the oldest event not yet decided; 0 with none.
    QueueOldestAge,
    /// Connections in the pool: `state` is `idle` or `used`.
    PoolConnections,
    /// The pool's configured maximum.
    PoolMax,
}

impl Gauge {
    const ALL: [Self; 4] = [
        Self::QueueBacklog,
        Self::QueueOldestAge,
        Self::PoolConnections,
        Self::PoolMax,
    ];

    /// The instrument's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::QueueBacklog => "openbilling.queue.backlog",
            Self::QueueOldestAge => "openbilling.queue.oldest_age",
            Self::PoolConnections => "db.client.connection.count",
            Self::PoolMax => "db.client.connection.max",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::QueueBacklog => "Usage events received and not yet decided",
            Self::QueueOldestAge => "Age of the oldest usage event not yet decided",
            Self::PoolConnections => "Connections in the PostgreSQL pool, by state",
            Self::PoolMax => "The most connections the PostgreSQL pool opens",
        }
    }

    const fn unit(self) -> &'static str {
        match self {
            Self::QueueBacklog => "{event}",
            Self::QueueOldestAge => "s",
            Self::PoolConnections | Self::PoolMax => "{connection}",
        }
    }
}

/// The name of the computed gauge: seconds since a drain pass last succeeded.
pub const DRAIN_LAG: &str = "openbilling.drain.lag";

/// The upper bounds of the duration buckets, in seconds.
pub const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The database operations a histogram names: a closed set, so a span's `db.operation` maps into it and anything
/// else is `other`.
pub const DB_OPERATIONS: [&str; 21] = [
    "now",
    "configure",
    "open_account",
    "set_debt_limit",
    "set_member_cap",
    "grant_allowance",
    "credit",
    "reverse",
    "intake",
    "drain",
    "balance",
    "allowed",
    "lots",
    "outcome",
    "daily_usage",
    "prepare_partitions",
    "queue_stats",
    "load_price_list",
    "price_versions",
    "rate",
    "intake_raw",
];

/// The closed label value of a `db.operation` span attribute.
#[must_use]
pub fn db_operation(name: &str) -> &'static str {
    DB_OPERATIONS
        .iter()
        .find(|known| **known == name)
        .copied()
        .unwrap_or("other")
}

/// One histogram's buckets.
#[derive(Clone, Debug, Default)]
pub struct Hist {
    /// Observations per bucket, the last is above every bound.
    pub counts: [u64; BUCKETS.len() + 1],
    /// The sum of the observations.
    pub sum: f64,
    /// The number of observations.
    pub count: u64,
}

type Key<I> = (I, Vec<Pair>);

#[derive(Default)]
struct State {
    counters: BTreeMap<Key<Counter>, u64>,
    histograms: BTreeMap<Key<Histogram>, Hist>,
    gauges: BTreeMap<Key<Gauge>, f64>,
}

/// Where the registry also sends each counter increment and histogram observation as it happens: the seam by which
/// `openbilling-telemetry` feeds the OpenTelemetry SDK's instruments, so that this crate, and the crates that record
/// into it, depend on no exporter (Rule 4). Gauges are read through [`Metrics::snapshot`] instead. An implementation
/// must not block and must not record into the registry it is attached to.
pub trait Recorder: Send + Sync {
    /// A counter was incremented by `n`.
    fn counter(&self, counter: Counter, labels: Labels<'_>, n: u64);
    /// A duration, in seconds, was observed.
    fn histogram(&self, histogram: Histogram, labels: Labels<'_>, seconds: f64);
}

/// The registry.
pub struct Metrics {
    recorder: OnceLock<Arc<dyn Recorder>>,
    state: Mutex<State>,
    started: SystemTime,
    born: Instant,
    /// Milliseconds since `born` at the last successful drain pass, plus one; 0 is never.
    last_drain_ok: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// A read of the registry, for the exporter and for tests.
pub struct Snapshot {
    /// When the registry began.
    pub start: SystemTime,
    /// Counters.
    pub counters: Vec<(Counter, Vec<Pair>, u64)>,
    /// Histograms.
    pub histograms: Vec<(Histogram, Vec<Pair>, Hist)>,
    /// Gauges.
    pub gauges: Vec<(Gauge, Vec<Pair>, f64)>,
    drain_lag: f64,
}

impl Snapshot {
    /// A counter's total over every label set, or the labels' when `labels` is `Some`.
    #[must_use]
    pub fn counter(&self, counter: Counter, labels: Option<Labels<'_>>) -> u64 {
        self.counters
            .iter()
            .filter(|(c, l, _)| *c == counter && labels.is_none_or(|want| want == l.as_slice()))
            .map(|(_, _, n)| *n)
            .sum()
    }

    /// A histogram's number of observations over every label set.
    #[must_use]
    pub fn observations(&self, histogram: Histogram) -> u64 {
        self.histograms
            .iter()
            .filter(|(h, _, _)| *h == histogram)
            .map(|(_, _, hist)| hist.count)
            .sum()
    }

    /// A gauge's value for these labels.
    #[must_use]
    pub fn gauge(&self, gauge: Gauge, labels: Labels<'_>) -> Option<f64> {
        self.gauges
            .iter()
            .find(|(g, l, _)| *g == gauge && l.as_slice() == labels)
            .map(|(_, _, v)| *v)
    }

    /// The seconds since a drain pass last succeeded.
    #[must_use]
    pub const fn drain_lag(&self) -> f64 {
        self.drain_lag
    }
}

impl Metrics {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            recorder: OnceLock::new(),
            state: Mutex::new(State::default()),
            started: SystemTime::now(),
            born: Instant::now(),
            last_drain_ok: AtomicU64::new(0),
        }
    }

    /// Attaches the recorder that also receives every counter increment and histogram observation from now on.
    /// Only the first call takes effect; `false` says a recorder was already attached.
    pub fn attach(&self, recorder: Arc<dyn Recorder>) -> bool {
        self.recorder.set(recorder).is_ok()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `n` to a counter.
    pub fn add(&self, counter: Counter, labels: Labels<'_>, n: u64) {
        if n == 0 {
            return;
        }
        {
            let mut state = self.lock();
            let total = state
                .counters
                .entry((counter, labels.to_vec()))
                .or_insert(0);
            *total = total.saturating_add(n);
        }
        if let Some(recorder) = self.recorder.get() {
            recorder.counter(counter, labels, n);
        }
    }

    /// Records a duration, in seconds, in a histogram.
    pub fn observe(&self, histogram: Histogram, labels: Labels<'_>, seconds: f64) {
        let seconds = if seconds.is_finite() && seconds >= 0.0 {
            seconds
        } else {
            0.0
        };
        let bucket = BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BUCKETS.len());
        {
            let mut state = self.lock();
            let hist = state
                .histograms
                .entry((histogram, labels.to_vec()))
                .or_default();
            hist.counts[bucket] += 1;
            hist.sum += seconds;
            hist.count += 1;
        }
        if let Some(recorder) = self.recorder.get() {
            recorder.histogram(histogram, labels, seconds);
        }
    }

    /// Sets a gauge.
    pub fn set(&self, gauge: Gauge, labels: Labels<'_>, value: f64) {
        self.lock().gauges.insert((gauge, labels.to_vec()), value);
    }

    /// Notes that a drain pass succeeded now: the drain's lag restarts from zero.
    pub fn drain_succeeded(&self) {
        let ms = u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
        self.last_drain_ok.store(ms + 1, Ordering::Relaxed);
    }

    /// The current values.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        let last = self.last_drain_ok.load(Ordering::Relaxed);
        let since = self.born.elapsed().as_secs_f64();
        // Never succeeded: the lag is the process's age, so a drain that never works shows as a growing lag.
        #[allow(clippy::cast_precision_loss)]
        let drain_lag = if last == 0 {
            since
        } else {
            (since - (last - 1) as f64 / 1000.0).max(0.0)
        };
        Snapshot {
            start: self.started,
            counters: state
                .counters
                .iter()
                .map(|((c, l), n)| (*c, l.clone(), *n))
                .collect(),
            histograms: state
                .histograms
                .iter()
                .map(|((h, l), v)| (*h, l.clone(), v.clone()))
                .collect(),
            gauges: state
                .gauges
                .iter()
                .map(|((g, l), v)| (*g, l.clone(), *v))
                .collect(),
            drain_lag,
        }
    }
}

/// The process-wide registry every crate records into.
#[must_use]
pub fn global() -> &'static Metrics {
    static GLOBAL: OnceLock<Metrics> = OnceLock::new();
    GLOBAL.get_or_init(Metrics::new)
}

/// The names the instruments have once a Prometheus-compatible backend has them (the OpenTelemetry to Prometheus
/// translation: dots to underscores, the unit as a suffix unless it is a `{annotation}`, `_total` on counters,
/// `_bucket`/`_sum`/`_count` on histograms). The dashboards in `deploy/dashboards` query these, and a test holds
/// every name they query to this list.
#[must_use]
pub fn exposed_names() -> Vec<String> {
    let base = |name: &str, unit: &str| {
        let mut out = name.replace('.', "_");
        if unit == "s" {
            out.push_str("_seconds");
        }
        out
    };
    let mut names = Vec::new();
    for counter in Counter::ALL {
        let (name, _, unit) = describe_counter(counter);
        names.push(format!("{}_total", base(name, unit)));
    }
    for histogram in Histogram::ALL {
        let (name, _) = describe_histogram(histogram);
        let base = base(name, "s");
        names.extend(["_bucket", "_sum", "_count"].map(|suffix| format!("{base}{suffix}")));
    }
    for gauge in Gauge::ALL {
        let (name, _, unit) = describe_gauge(gauge);
        names.push(base(name, unit));
    }
    names.push(base(DRAIN_LAG, "s"));
    names
}

#[doc(hidden)]
pub const fn all_counters() -> [Counter; 8] {
    Counter::ALL
}
#[doc(hidden)]
pub fn describe_counter(c: Counter) -> (&'static str, &'static str, &'static str) {
    (c.name(), c.description(), c.unit())
}
#[doc(hidden)]
pub fn describe_histogram(h: Histogram) -> (&'static str, &'static str) {
    (h.name(), h.description())
}
#[doc(hidden)]
pub fn describe_gauge(g: Gauge) -> (&'static str, &'static str, &'static str) {
    (g.name(), g.description(), g.unit())
}
#[doc(hidden)]
pub const fn all_histograms() -> [Histogram; 3] {
    Histogram::ALL
}
#[doc(hidden)]
pub const fn all_gauges() -> [Gauge; 4] {
    Gauge::ALL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_and_histograms_bucket() {
        let m = Metrics::new();
        m.add(Counter::DrainRuns, &[("result", "ok")], 2);
        m.add(Counter::DrainRuns, &[("result", "ok")], 1);
        m.add(Counter::DrainRuns, &[("result", "error")], 1);
        m.add(Counter::Errors, &[("class", "invalid")], 0);
        m.observe(Histogram::Drain, &[], 0.004);
        m.observe(Histogram::Drain, &[], 99.0);
        m.observe(Histogram::Drain, &[], f64::NAN);
        let s = m.snapshot();
        assert_eq!(s.counter(Counter::DrainRuns, Some(&[("result", "ok")])), 3);
        assert_eq!(s.counter(Counter::DrainRuns, None), 4);
        assert_eq!(s.counter(Counter::Errors, None), 0);
        assert_eq!(s.observations(Histogram::Drain), 3);
        let h = &s.histograms[0].2;
        assert_eq!((h.counts[0], h.counts[BUCKETS.len()]), (2, 1));
    }

    #[test]
    fn an_attached_recorder_sees_counters_and_histograms_once() {
        use std::sync::atomic::AtomicU64;
        #[derive(Default)]
        struct Seen(AtomicU64, AtomicU64);
        impl Recorder for Seen {
            fn counter(&self, _: Counter, _: Labels<'_>, n: u64) {
                self.0.fetch_add(n, Ordering::Relaxed);
            }
            fn histogram(&self, _: Histogram, _: Labels<'_>, _: f64) {
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        let seen = Arc::new(Seen::default());
        let m = Metrics::new();
        m.add(Counter::DrainRuns, &[], 5);
        assert!(m.attach(seen.clone()));
        assert!(!m.attach(Arc::new(Seen::default())), "the first one stays");
        m.add(Counter::DrainRuns, &[], 2);
        m.add(Counter::DrainRuns, &[], 0);
        m.observe(Histogram::Drain, &[], 0.1);
        assert_eq!(seen.0.load(Ordering::Relaxed), 2);
        assert_eq!(seen.1.load(Ordering::Relaxed), 1);
        assert_eq!(m.snapshot().counter(Counter::DrainRuns, None), 7);
    }

    #[test]
    fn the_drain_lag_restarts_at_a_success() {
        let m = Metrics::new();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(m.snapshot().drain_lag() >= 0.03);
        m.drain_succeeded();
        assert!(m.snapshot().drain_lag() < 0.03);
    }

    #[test]
    fn exposed_names_follow_the_prometheus_translation() {
        let names = exposed_names();
        for want in [
            "openbilling_intake_events_total",
            "http_server_request_duration_seconds_bucket",
            "db_client_operation_duration_seconds_count",
            "openbilling_queue_oldest_age_seconds",
            "db_client_connection_count",
            "openbilling_drain_lag_seconds",
        ] {
            assert!(names.iter().any(|n| n == want), "{want}");
        }
    }

    #[test]
    fn a_database_operation_is_one_of_a_closed_set() {
        assert_eq!(db_operation("intake"), "intake");
        assert_eq!(db_operation("acc_42"), "other");
    }
}
