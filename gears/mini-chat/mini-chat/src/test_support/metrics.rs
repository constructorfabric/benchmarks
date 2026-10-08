//! [`MetricsProbe`]: the gear's instruments on an in-memory OpenTelemetry exporter, so tests can
//! read what was recorded.

use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, Metric, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::metrics::Metrics;

/// Instruments (default prefix `mini_chat`) whose recordings can be read back.
pub struct MetricsProbe {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    pub metrics: Arc<Metrics>,
}

impl MetricsProbe {
    pub fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = Arc::new(Metrics::with_meter(&provider.meter("test"), ""));
        Self {
            provider,
            exporter,
            metrics,
        }
    }

    /// Total of counter `mini_chat_{name}` over the data points carrying every `labels` pair.
    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        self.provider.force_flush().expect("flush metrics");
        let full = format!("mini_chat_{name}");
        let exported = self.exporter.get_finished_metrics().expect("metrics");
        // Cumulative temporality: the latest export that has the counter holds its total.
        let Some(metric_data) = exported
            .iter()
            .rev()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == full)
            .map(Metric::data)
        else {
            return 0;
        };
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric_data else {
            panic!("{full} is not a u64 counter");
        };
        sum.data_points()
            .filter(|p| {
                labels.iter().all(|(k, v)| {
                    p.attributes()
                        .any(|kv| kv.key.as_str() == *k && kv.value.as_str() == *v)
                })
            })
            .map(SumDataPoint::value)
            .sum()
    }
}
