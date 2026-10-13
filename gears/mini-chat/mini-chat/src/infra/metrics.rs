//! OpenTelemetry metrics (exported over OTLP by `ToolKit`). Instrument names
//! use the configurable prefix (default `mini_chat`); labels never carry
//! tenant/user/chat/request identifiers.

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// Metrics facade.
pub struct Metrics {
    prefix: String,
    meter: Meter,
}

impl Metrics {
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let prefix = if prefix.trim().is_empty() {
            "mini_chat".to_owned()
        } else {
            prefix.trim().to_owned()
        };
        Self {
            prefix,
            meter: opentelemetry::global::meter("mini-chat"),
        }
    }

    fn name(&self, n: &str) -> String {
        format!("{}_{n}", self.prefix)
    }

    /// Increments counter `n` by `by`.
    pub fn inc(&self, n: &str, by: u64, labels: &[(&'static str, String)]) {
        let attrs: Vec<KeyValue> = labels
            .iter()
            .map(|(k, v)| KeyValue::new(*k, v.clone()))
            .collect();
        self.meter.u64_counter(self.name(n)).build().add(by, &attrs);
    }

    /// Adds `delta` to up-down counter `n`.
    pub fn updown(&self, n: &str, delta: i64) {
        self.meter.i64_up_down_counter(self.name(n)).build().add(delta, &[]);
    }

    /// Records value `v` in histogram `n`.
    pub fn record(&self, n: &str, v: f64, labels: &[(&'static str, String)]) {
        let attrs: Vec<KeyValue> = labels
            .iter()
            .map(|(k, v)| KeyValue::new(*k, v.clone()))
            .collect();
        self.meter.f64_histogram(self.name(n)).build().record(v, &attrs);
    }
}
