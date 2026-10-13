//! OpenTelemetry metrics (`mini_chat_*`). No-op until the host installs an exporter.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

fn meter() -> &'static Meter {
    static METER: OnceLock<Meter> = OnceLock::new();
    METER.get_or_init(|| {
        let scope = opentelemetry::InstrumentationScope::builder("mini_chat").build();
        opentelemetry::global::meter_with_scope(scope)
    })
}

/// Increments counter `name` by `value` with the given labels.
pub fn incr(name: &'static str, value: u64, labels: &[(&'static str, String)]) {
    let attrs: Vec<KeyValue> = labels.iter().map(|(k, v)| KeyValue::new(*k, v.clone())).collect();
    meter().u64_counter(name).build().add(value, &attrs);
}

/// Records a histogram sample.
pub fn record(name: &'static str, value: f64, labels: &[(&'static str, String)]) {
    let attrs: Vec<KeyValue> = labels.iter().map(|(k, v)| KeyValue::new(*k, v.clone())).collect();
    meter().f64_histogram(name).build().record(value, &attrs);
}
