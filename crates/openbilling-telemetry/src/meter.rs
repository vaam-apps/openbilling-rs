//! The registry's counters and histograms as OpenTelemetry instruments. [`install`] attaches a
//! [`Recorder`](openbilling_metrics::Recorder) to the registry that adds each increment and observation to the SDK's
//! instrument of the same name, and registers the gauges (and the drain's lag) as observable instruments that read
//! the registry when the SDK collects. Names, units and descriptions are the registry's own, so the dashboards'
//! names do not depend on this file.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter as OtelCounter, Histogram as OtelHistogram, Meter};

use openbilling_metrics::{
    BUCKETS, Counter, DRAIN_LAG, Histogram, Labels, Metrics, Recorder, all_counters, all_gauges,
    all_histograms, describe_counter, describe_gauge, describe_histogram,
};

fn attributes(labels: Labels<'_>) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(key, value)| KeyValue::new(*key, *value))
        .collect()
}

struct Bridge {
    counters: Vec<(Counter, OtelCounter<u64>)>,
    histograms: Vec<(Histogram, OtelHistogram<f64>)>,
}

impl Recorder for Bridge {
    fn counter(&self, counter: Counter, labels: Labels<'_>, n: u64) {
        if let Some((_, instrument)) = self.counters.iter().find(|(c, _)| *c == counter) {
            instrument.add(n, &attributes(labels));
        }
    }

    fn histogram(&self, histogram: Histogram, labels: Labels<'_>, seconds: f64) {
        if let Some((_, instrument)) = self.histograms.iter().find(|(h, _)| *h == histogram) {
            instrument.record(seconds, &attributes(labels));
        }
    }
}

/// Creates the instruments on `meter` and attaches them to `metrics`. Returns `false` when the registry already had
/// a recorder.
pub fn install(meter: &Meter, metrics: &'static Metrics) -> bool {
    let counters = all_counters()
        .into_iter()
        .map(|counter| {
            let (name, description, unit) = describe_counter(counter);
            let instrument = meter
                .u64_counter(name)
                .with_description(description)
                .with_unit(unit)
                .build();
            (counter, instrument)
        })
        .collect();
    let histograms = all_histograms()
        .into_iter()
        .map(|histogram| {
            let (name, description) = describe_histogram(histogram);
            let instrument = meter
                .f64_histogram(name)
                .with_description(description)
                .with_unit("s")
                .with_boundaries(BUCKETS.to_vec())
                .build();
            (histogram, instrument)
        })
        .collect();
    for gauge in all_gauges() {
        let (name, description, unit) = describe_gauge(gauge);
        let _ = meter
            .f64_observable_gauge(name)
            .with_description(description)
            .with_unit(unit)
            .with_callback(move |observer| {
                for (g, labels, value) in metrics.snapshot().gauges {
                    if g == gauge {
                        observer.observe(value, &attributes(&labels));
                    }
                }
            })
            .build();
    }
    let _ = meter
        .f64_observable_gauge(DRAIN_LAG)
        .with_description("Seconds since a drain pass last succeeded")
        .with_unit("s")
        .with_callback(move |observer| observer.observe(metrics.snapshot().drain_lag(), &[]))
        .build();
    metrics.attach(Arc::new(Bridge {
        counters,
        histograms,
    }))
}
