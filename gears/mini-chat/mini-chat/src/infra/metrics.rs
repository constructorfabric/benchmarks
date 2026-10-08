//! OpenTelemetry metrics of the gear (exported over OTLP by the host).
//! Instruments are created lazily by name and cached; names use the
//! configured prefix (default `mini_chat`). Labels never carry tenant, user,
//! chat or request identifiers.

use std::collections::HashMap;
use std::sync::Mutex;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

pub struct Metrics {
    prefix: String,
    meter: Meter,
    counters: Mutex<HashMap<String, Counter<u64>>>,
    histograms: Mutex<HashMap<String, Histogram<f64>>>,
    gauges: Mutex<HashMap<String, UpDownCounter<i64>>>,
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
        Self {
            prefix: prefix.to_owned(),
            meter: opentelemetry::global::meter("mini-chat"),
            counters: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
            gauges: Mutex::new(HashMap::new()),
        }
    }

    fn name(&self, n: &str) -> String {
        format!("{}_{n}", self.prefix)
    }

    /// Increment a counter by `v`.
    pub fn add(&self, name: &str, v: u64, labels: &[(&'static str, &str)]) {
        let full = self.name(name);
        let c = {
            let Ok(mut g) = self.counters.lock() else {
                return;
            };
            g.entry(full.clone())
                .or_insert_with(|| self.meter.u64_counter(full).build())
                .clone()
        };
        c.add(v, &kv(labels));
    }

    pub fn inc(&self, name: &str, labels: &[(&'static str, &str)]) {
        self.add(name, 1, labels);
    }

    /// Record a histogram value.
    pub fn record(&self, name: &str, v: f64, labels: &[(&'static str, &str)]) {
        let full = self.name(name);
        let h = {
            let Ok(mut g) = self.histograms.lock() else {
                return;
            };
            g.entry(full.clone())
                .or_insert_with(|| self.meter.f64_histogram(full).build())
                .clone()
        };
        h.record(v, &kv(labels));
    }

    /// Change an up-down counter.
    pub fn gauge_add(&self, name: &str, v: i64) {
        let full = self.name(name);
        let g = {
            let Ok(mut m) = self.gauges.lock() else {
                return;
            };
            m.entry(full.clone())
                .or_insert_with(|| self.meter.i64_up_down_counter(full).build())
                .clone()
        };
        g.add(v, &[]);
    }
}
