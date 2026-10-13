//! Instrument names (prefix, no `_total`) and label values.

use std::time::Duration;

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    Temporality,
};

use super::{MiniChatMetrics, SCOPE};

fn recorder(prefix: &str) -> (SdkMeterProvider, InMemoryMetricExporter, MiniChatMetrics) {
    let exporter = InMemoryMetricExporterBuilder::new()
        .with_temporality(Temporality::Cumulative)
        .build();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = MiniChatMetrics::new(&provider.meter(SCOPE), prefix);
    (provider, exporter, metrics)
}

/// Sum of the `u64` counter `name` over the points carrying every `labels` pair.
fn counter(exporter: &InMemoryMetricExporter, name: &str, labels: &[(&str, &str)]) -> u64 {
    let mut total = 0;
    for rm in exporter.get_finished_metrics().unwrap() {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() != name {
                    continue;
                }
                if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                    total += sum
                        .data_points()
                        .filter(|dp| {
                            labels.iter().all(|(k, v)| {
                                dp.attributes()
                                    .any(|kv| kv.key.as_str() == *k && kv.value.as_str() == *v)
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum::<u64>();
                }
            }
        }
    }
    total
}

/// Names of every exported histogram.
fn histogram_names(exporter: &InMemoryMetricExporter) -> Vec<String> {
    let mut out = Vec::new();
    for rm in exporter.get_finished_metrics().unwrap() {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if matches!(
                    metric.data(),
                    AggregatedMetrics::F64(MetricData::Histogram(_))
                ) {
                    out.push(metric.name().to_owned());
                }
            }
        }
    }
    out
}

#[test]
fn orphan_and_reaper_series_use_the_prefix_and_labels() {
    let (provider, exporter, m) = recorder("mc");
    m.orphan_detected();
    m.orphan_detected();
    m.orphan_finalized();
    m.orphan_scan_duration(Duration::from_millis(12));
    m.upload_abandoned("pending");
    m.upload_abandoned("uploaded");
    m.upload_abandoned("uploaded");
    m.upload_reaper_scan_duration(Duration::from_millis(3));
    provider.force_flush().unwrap();

    let reason = [("reason", "stale_progress")];
    assert_eq!(counter(&exporter, "mc_orphan_detected", &reason), 2);
    assert_eq!(counter(&exporter, "mc_orphan_finalized", &reason), 1);
    assert_eq!(
        counter(
            &exporter,
            "mc_streams_aborted",
            &[("trigger", "orphan_timeout")]
        ),
        1
    );
    assert_eq!(
        counter(
            &exporter,
            "mc_attachment_upload_abandoned",
            &[("from_status", "uploaded")]
        ),
        2
    );
    assert_eq!(
        counter(
            &exporter,
            "mc_attachment_upload_abandoned",
            &[("from_status", "pending")]
        ),
        1
    );
    let hist = histogram_names(&exporter);
    assert!(
        hist.contains(&"mc_orphan_scan_duration_seconds".to_owned()),
        "{hist:?}"
    );
    assert!(
        hist.contains(&"mc_upload_reaper_scan_duration_seconds".to_owned()),
        "{hist:?}"
    );
}

#[test]
fn outcome_series_carry_result_labels() {
    let (provider, exporter, m) = recorder("mini_chat");
    m.audit_emit("dropped");
    m.background_indexing("set_ready_failed");
    m.summary_trigger("not_needed");
    m.summary_execution("success");
    m.cleanup_retry("file", "provider_error");
    m.stream_started("openai", "gpt-4.1");
    m.stream_completed("openai", "gpt-4.1", Some("max_output_tokens"));
    provider.force_flush().unwrap();

    assert_eq!(
        counter(&exporter, "mini_chat_audit_emit", &[("result", "dropped")]),
        1
    );
    assert_eq!(
        counter(
            &exporter,
            "mini_chat_attachment_background_indexing",
            &[("result", "set_ready_failed")]
        ),
        1
    );
    assert_eq!(
        counter(
            &exporter,
            "mini_chat_thread_summary_trigger",
            &[("result", "not_needed")]
        ),
        1
    );
    assert_eq!(
        counter(
            &exporter,
            "mini_chat_thread_summary_execution",
            &[("result", "success")]
        ),
        1
    );
    assert_eq!(
        counter(
            &exporter,
            "mini_chat_cleanup_retry",
            &[("resource_type", "file"), ("reason", "provider_error")]
        ),
        1
    );
    let model = [("provider", "openai"), ("model", "gpt-4.1")];
    assert_eq!(counter(&exporter, "mini_chat_stream_started", &model), 1);
    // An incomplete stream counts as completed and incomplete.
    assert_eq!(counter(&exporter, "mini_chat_stream_completed", &model), 1);
    assert_eq!(
        counter(
            &exporter,
            "mini_chat_stream_incomplete",
            &[("reason", "max_output_tokens")]
        ),
        1
    );
}

#[test]
fn noop_instruments_accept_records() {
    let m = MiniChatMetrics::noop();
    m.orphan_finalized();
    m.upload_abandoned("pending");
    m.finalization_latency(Duration::from_millis(1));
}

/// Current value of the `i64` up-down counter `name`.
fn up_down(exporter: &InMemoryMetricExporter, name: &str) -> i64 {
    let mut last = 0;
    for rm in exporter.get_finished_metrics().unwrap() {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Sum(sum)) = metric.data()
                {
                    last = sum
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    last
}

#[test]
fn pending_attachment_guard_balances_the_up_down_counter() {
    let (provider, exporter, m) = recorder("mini_chat");
    let m = std::sync::Arc::new(m);
    let first = m.attachment_pending();
    let second = m.attachment_pending();
    provider.force_flush().unwrap();
    assert_eq!(up_down(&exporter, "mini_chat_attachments_pending"), 2);
    drop(first);
    drop(second);
    exporter.reset();
    provider.force_flush().unwrap();
    assert_eq!(up_down(&exporter, "mini_chat_attachments_pending"), 0);
}

#[test]
fn ttft_overhead_carries_provider_and_model() {
    let (provider, exporter, m) = recorder("mini_chat");
    m.ttft_overhead("openai", "gpt-4.1", Duration::from_millis(3));
    provider.force_flush().unwrap();
    assert!(histogram_names(&exporter).contains(&"mini_chat_ttft_overhead_ms".to_owned()));
}
