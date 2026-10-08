//! OpenTelemetry instruments (`metrics.prefix`, default `mini_chat`). No-op when no
//! meter provider is installed.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter};

/// Gear counters.
#[derive(Clone)]
pub struct Metrics {
    turns: Counter<u64>,
    replays: Counter<u64>,
    quota_rejections: Counter<u64>,
    downgrades: Counter<u64>,
    uploads: Counter<u64>,
}

impl Metrics {
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        Self {
            turns: meter
                .u64_counter(format!("{prefix}_turns_total"))
                .with_description("Finalized turns by terminal state and error code")
                .build(),
            replays: meter
                .u64_counter(format!("{prefix}_replays_total"))
                .with_description("Idempotent replays served without a provider call")
                .build(),
            quota_rejections: meter
                .u64_counter(format!("{prefix}_quota_rejections_total"))
                .with_description("429 quota rejections by subject")
                .build(),
            downgrades: meter
                .u64_counter(format!("{prefix}_downgrades_total"))
                .with_description("Tier downgrades by reason")
                .build(),
            uploads: meter
                .u64_counter(format!("{prefix}_uploads_total"))
                .with_description("Attachment uploads by kind and outcome")
                .build(),
        }
    }

    /// Instruments on the global meter provider.
    #[must_use]
    pub fn global(prefix: &str) -> Self {
        Self::new(&opentelemetry::global::meter("mini-chat"), prefix)
    }

    pub fn turn_finalized(&self, state: &str, error_code: Option<&str>) {
        self.turns.add(
            1,
            &[
                KeyValue::new("state", state.to_owned()),
                KeyValue::new("error_code", error_code.unwrap_or("none").to_owned()),
            ],
        );
    }

    pub fn replay(&self) {
        self.replays.add(1, &[]);
    }

    pub fn quota_rejected(&self, subject: &str) {
        self.quota_rejections
            .add(1, &[KeyValue::new("subject", subject.to_owned())]);
    }

    pub fn downgraded(&self, reason: &str) {
        self.downgrades
            .add(1, &[KeyValue::new("reason", reason.to_owned())]);
    }

    pub fn upload(&self, kind: &str, outcome: &str) {
        self.uploads.add(
            1,
            &[
                KeyValue::new("kind", kind.to_owned()),
                KeyValue::new("outcome", outcome.to_owned()),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::Metrics;

    #[test]
    fn instruments_record_with_prefix() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let m = Metrics::new(&provider.meter("t"), "mini_chat");
        m.turn_finalized("completed", None);
        m.replay();
        m.quota_rejected("tokens");
        m.downgraded("premium_quota_exhausted");
        m.upload("document", "ready");
        provider.force_flush().unwrap();
        let names: Vec<String> = exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|rm| {
                rm.scope_metrics()
                    .flat_map(|sm| sm.metrics().map(|m| m.name().to_owned()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for n in [
            "mini_chat_turns_total",
            "mini_chat_replays_total",
            "mini_chat_quota_rejections_total",
            "mini_chat_downgrades_total",
            "mini_chat_uploads_total",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} missing in {names:?}");
        }
    }

    #[test]
    fn global_meter_without_provider_is_noop() {
        let m = Metrics::global("mini_chat");
        m.turn_finalized("failed", Some("provider_error"));
    }
}
