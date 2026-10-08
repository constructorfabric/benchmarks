//! OpenTelemetry metrics (`mini_chat_*`, exported over OTLP by the platform).

use std::collections::HashMap;
use std::sync::Mutex;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Lazily created instruments keyed by their full name.
pub struct Metrics {
    meter: Meter,
    prefix: String,
    counters: Mutex<HashMap<String, Counter<u64>>>,
    histograms: Mutex<HashMap<String, Histogram<f64>>>,
    updowns: Mutex<HashMap<String, UpDownCounter<i64>>>,
}

fn kv(labels: &[(&'static str, &str)]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(k, v)| KeyValue::new(*k, (*v).to_owned()))
        .collect()
}

impl Metrics {
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("mini_chat").build();
        let prefix = if prefix.trim().is_empty() {
            "mini_chat".to_owned()
        } else {
            prefix.trim().to_owned()
        };
        Self {
            meter: opentelemetry::global::meter_with_scope(scope),
            prefix,
            counters: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
            updowns: Mutex::new(HashMap::new()),
        }
    }

    fn name(&self, n: &str) -> String {
        format!("{}_{n}", self.prefix)
    }

    /// Increments a counter by `n`.
    pub fn add(&self, name: &str, n: u64, labels: &[(&'static str, &str)]) {
        let full = self.name(name);
        let Ok(mut map) = self.counters.lock() else {
            return;
        };
        let c = map
            .entry(full.clone())
            .or_insert_with(|| self.meter.u64_counter(full).build());
        c.add(n, &kv(labels));
    }

    /// Increments a counter by one.
    pub fn inc(&self, name: &str, labels: &[(&'static str, &str)]) {
        self.add(name, 1, labels);
    }

    /// Records a histogram value.
    pub fn record(&self, name: &str, v: f64, labels: &[(&'static str, &str)]) {
        let full = self.name(name);
        let Ok(mut map) = self.histograms.lock() else {
            return;
        };
        let h = map
            .entry(full.clone())
            .or_insert_with(|| self.meter.f64_histogram(full).build());
        h.record(v, &kv(labels));
    }

    /// Adds to an up-down counter.
    pub fn updown(&self, name: &str, delta: i64) {
        let full = self.name(name);
        let Ok(mut map) = self.updowns.lock() else {
            return;
        };
        let c = map
            .entry(full.clone())
            .or_insert_with(|| self.meter.i64_up_down_counter(full).build());
        c.add(delta, &[]);
    }
}
