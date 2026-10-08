//! In-memory OpenTelemetry recorder for [`TestAppBuilder::metrics`].

use std::sync::Arc;

use mini_chat::infra::metrics::{MiniChatMetrics, SCOPE};
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, HistogramDataPoint, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    Temporality,
};

/// Instruments (default prefix `mini_chat`) on an SDK provider with an
/// in-memory exporter (cumulative temporality).
pub struct MetricsRecorder {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    pub metrics: Arc<MiniChatMetrics>,
}

impl MetricsRecorder {
    pub fn new() -> Self {
        let exporter = InMemoryMetricExporterBuilder::new()
            .with_temporality(Temporality::Cumulative)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = Arc::new(MiniChatMetrics::new(&provider.meter(SCOPE), "mini_chat"));
        Self {
            provider,
            exporter,
            metrics,
        }
    }

    /// `f` applied to the last exported data of instrument `name`.
    fn latest<T>(&self, name: &str, f: impl Fn(&AggregatedMetrics) -> Option<T>) -> Option<T> {
        self.exporter.reset();
        self.provider.force_flush().expect("flush metrics");
        let all = self
            .exporter
            .get_finished_metrics()
            .expect("finished metrics");
        all.iter()
            .rev()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == name)
            .and_then(|m| f(m.data()))
    }

    /// Sum of the `u64` counter `name` (0 when never recorded).
    pub fn counter(&self, name: &str) -> u64 {
        self.latest(name, |d| match d {
            AggregatedMetrics::U64(MetricData::Sum(s)) => {
                Some(s.data_points().map(SumDataPoint::value).sum())
            }
            _ => None,
        })
        .unwrap_or(0)
    }

    /// Current value of the `i64` up-down counter `name` (0 when never recorded).
    pub fn up_down(&self, name: &str) -> i64 {
        self.latest(name, |d| match d {
            AggregatedMetrics::I64(MetricData::Sum(s)) => {
                Some(s.data_points().map(SumDataPoint::value).sum())
            }
            _ => None,
        })
        .unwrap_or(0)
    }

    /// `(count, label pairs of the first point)` of the `f64` histogram `name`.
    pub fn histogram(&self, name: &str) -> (u64, Vec<(String, String)>) {
        self.latest(name, |d| match d {
            AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                let count = h.data_points().map(HistogramDataPoint::count).sum();
                let labels = h
                    .data_points()
                    .next()
                    .map(|p| {
                        p.attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((count, labels))
            }
            _ => None,
        })
        .unwrap_or_default()
    }
}
