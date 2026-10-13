//! `mini_chat_*` OpenTelemetry instruments (exported over OTLP by the platform).

use std::collections::HashMap;
use std::sync::Mutex;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Lazily created instruments keyed by name.
pub struct Metrics {
    prefix: String,
    meter: Meter,
    counters: Mutex<HashMap<String, Counter<u64>>>,
    histograms: Mutex<HashMap<String, Histogram<f64>>>,
    gauges: Mutex<HashMap<String, UpDownCounter<i64>>>,
}

fn kv(labels: &[(&'static str, &str)]) -> Vec<KeyValue> {
    labels.iter().map(|(k, v)| KeyValue::new(*k, (*v).to_owned())).collect()
}

impl Metrics {
    /// New metrics with a name prefix (`mini_chat` by default).
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            meter: opentelemetry::global::meter("mini-chat"),
            counters: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
            gauges: Mutex::new(HashMap::new()),
        }
    }

    fn full(&self, name: &str) -> String {
        format!("{}_{name}", self.prefix)
    }

    /// Increments a counter.
    pub fn inc(&self, name: &str, labels: &[(&'static str, &str)]) {
        self.add(name, 1, labels);
    }

    /// Adds to a counter.
    pub fn add(&self, name: &str, value: u64, labels: &[(&'static str, &str)]) {
        let full = self.full(name);
        if let Ok(mut m) = self.counters.lock() {
            let c = m.entry(full.clone()).or_insert_with(|| self.meter.u64_counter(full).build());
            c.add(value, &kv(labels));
        }
    }

    /// Records a histogram value.
    pub fn record(&self, name: &str, value: f64, labels: &[(&'static str, &str)]) {
        let full = self.full(name);
        if let Ok(mut m) = self.histograms.lock() {
            let h = m.entry(full.clone()).or_insert_with(|| self.meter.f64_histogram(full).build());
            h.record(value, &kv(labels));
        }
    }

    /// Adds to an up-down counter.
    pub fn gauge_add(&self, name: &str, delta: i64) {
        let full = self.full(name);
        if let Ok(mut m) = self.gauges.lock() {
            let g = m.entry(full.clone()).or_insert_with(|| self.meter.i64_up_down_counter(full).build());
            g.add(delta, &[]);
        }
    }
}
