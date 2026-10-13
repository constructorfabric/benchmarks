#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the attachment instruments
//! (`cpt-cf-mini-chat-algo-attachments-record-metrics`,
//! `cpt-cf-mini-chat-dod-attachments-metrics`) recorded by `MiniChatMetrics`
//! through the `AttachmentMetrics` port, read back with the in-memory exporter.

use std::collections::{BTreeMap, BTreeSet};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::MiniChatMetrics;

type Labels = BTreeMap<String, String>;

struct Harness {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    metrics: MiniChatMetrics,
}

fn harness(prefix: &str) -> Harness {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = MiniChatMetrics::new(&provider.meter("mini_chat"), prefix);
    Harness {
        provider,
        exporter,
        metrics,
    }
}

fn labels<'a>(attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Labels {
    attributes
        .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
        .collect()
}

fn label_set(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

impl Harness {
    fn latest(&self) -> Vec<opentelemetry_sdk::metrics::data::ResourceMetrics> {
        self.provider.force_flush().expect("flush");
        self.exporter
            .get_finished_metrics()
            .unwrap()
            .into_iter()
            .last()
            .into_iter()
            .collect()
    }

    fn counter(&self, name: &str) -> Vec<(Labels, u64)> {
        let mut out = Vec::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics().filter(|m| m.name() == name) {
                    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                        panic!("`{name}` is not a u64 sum");
                    };
                    for point in sum.data_points() {
                        out.push((labels(point.attributes()), point.value()));
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn up_down(&self, name: &str) -> Vec<(Labels, i64)> {
        let mut out = Vec::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics().filter(|m| m.name() == name) {
                    let AggregatedMetrics::I64(MetricData::Sum(sum)) = metric.data() else {
                        panic!("`{name}` is not an i64 sum");
                    };
                    for point in sum.data_points() {
                        out.push((labels(point.attributes()), point.value()));
                    }
                }
            }
        }
        out
    }

    fn histogram(&self, name: &str) -> Vec<(Labels, u64, f64)> {
        let mut out = Vec::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics().filter(|m| m.name() == name) {
                    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data()
                    else {
                        panic!("`{name}` is not an f64 histogram");
                    };
                    for point in histogram.data_points() {
                        out.push((labels(point.attributes()), point.count(), point.sum()));
                    }
                }
            }
        }
        out
    }

    fn exported_names(&self) -> BTreeSet<String> {
        self.latest()
            .iter()
            .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .map(|metric| metric.name().to_owned())
            .collect()
    }

    fn all_label_keys(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                keys.extend(labels(point.attributes()).into_keys());
                            }
                        }
                        AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                keys.extend(labels(point.attributes()).into_keys());
                            }
                        }
                        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                            for point in histogram.data_points() {
                                keys.extend(labels(point.attributes()).into_keys());
                            }
                        }
                        other => panic!("unexpected aggregation: {other:?}"),
                    }
                }
            }
        }
        keys
    }
}

use crate::domain::service::attachments::model::{AttachmentKind, AttachmentStatus};
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, BackgroundResult, UploadResult,
};

fn kind(name: &str) -> Labels {
    label_set(&[("kind", name)])
}

#[test]
fn the_upload_counter_carries_kind_and_result() {
    let h = harness("");
    h.metrics.upload(AttachmentKind::Document, UploadResult::Ok);
    h.metrics.upload(AttachmentKind::Document, UploadResult::Ok);
    h.metrics
        .upload(AttachmentKind::Image, UploadResult::Rejected);
    h.metrics
        .upload(AttachmentKind::Document, UploadResult::Failed);

    assert_eq!(
        h.counter("mini_chat_attachment_upload"),
        vec![
            (label_set(&[("kind", "document"), ("result", "failed")]), 1),
            (label_set(&[("kind", "document"), ("result", "ok")]), 2),
            (label_set(&[("kind", "image"), ("result", "rejected")]), 1),
        ]
    );
}

#[test]
fn the_upload_size_is_a_histogram_by_kind() {
    let h = harness("");
    h.metrics.upload_bytes(AttachmentKind::Document, 1000);
    h.metrics.upload_bytes(AttachmentKind::Document, 500);
    h.metrics.upload_bytes(AttachmentKind::Image, 7);

    let mut points = h.histogram("mini_chat_attachment_upload_bytes");
    points.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(
        points,
        vec![(kind("document"), 2, 1500.0), (kind("image"), 1, 7.0)]
    );
}

#[test]
fn the_pending_gauge_goes_up_and_down_without_labels() {
    let h = harness("");
    h.metrics.pending_add(1);
    h.metrics.pending_add(1);
    h.metrics.pending_add(-1);

    assert_eq!(
        h.up_down("mini_chat_attachments_pending"),
        vec![(Labels::new(), 1)]
    );
}

#[test]
fn the_background_indexing_counter_has_its_four_results() {
    let h = harness("");
    for result in [
        BackgroundResult::Ready,
        BackgroundResult::Failed,
        BackgroundResult::Timeout,
        BackgroundResult::SetReadyFailed,
        BackgroundResult::Ready,
    ] {
        h.metrics.background_indexing(result);
    }

    assert_eq!(
        h.counter("mini_chat_attachment_background_indexing"),
        vec![
            (label_set(&[("result", "failed")]), 1),
            (label_set(&[("result", "ready")]), 2),
            (label_set(&[("result", "set_ready_failed")]), 1),
            (label_set(&[("result", "timeout")]), 1),
        ]
    );
}

#[test]
fn the_reaper_counts_abandoned_rows_by_status_and_times_its_scans() {
    let h = harness("");
    h.metrics.upload_abandoned(AttachmentStatus::Pending);
    h.metrics.upload_abandoned(AttachmentStatus::Uploaded);
    h.metrics.upload_abandoned(AttachmentStatus::Uploaded);
    h.metrics.reaper_scan_seconds(0.25);
    h.metrics.reaper_scan_seconds(0.75);

    assert_eq!(
        h.counter("mini_chat_attachment_upload_abandoned"),
        vec![
            (label_set(&[("from_status", "pending")]), 1),
            (label_set(&[("from_status", "uploaded")]), 2),
        ]
    );
    assert_eq!(
        h.histogram("mini_chat_upload_reaper_scan_duration_seconds"),
        vec![(Labels::new(), 2, 1.0)]
    );
}

#[test]
fn the_file_cleanup_counters_carry_the_resource_type_and_the_retry_reason() {
    let h = harness("");
    h.metrics.cleanup_completed();
    h.metrics.cleanup_completed();
    h.metrics.cleanup_failed();
    h.metrics.cleanup_retry();
    h.metrics.secondary_cleanup_skipped();

    assert_eq!(
        h.counter("mini_chat_cleanup_completed"),
        vec![(label_set(&[("resource_type", "file")]), 2)]
    );
    assert_eq!(
        h.counter("mini_chat_cleanup_failed"),
        vec![(label_set(&[("resource_type", "file")]), 1)]
    );
    assert_eq!(
        h.counter("mini_chat_cleanup_retry"),
        vec![(
            label_set(&[("reason", "provider_error"), ("resource_type", "file")]),
            1
        )]
    );
    assert_eq!(
        h.counter("mini_chat_secondary_cleanup_skipped"),
        vec![(label_set(&[("provider_kind", "anthropic")]), 1)]
    );
}

#[test]
fn a_configured_prefix_replaces_mini_chat() {
    let h = harness("acme");
    h.metrics.upload(AttachmentKind::Document, UploadResult::Ok);
    h.metrics.cleanup_completed();

    assert_eq!(h.counter("acme_attachment_upload").len(), 1);
    assert_eq!(h.counter("acme_cleanup_completed").len(), 1);
    assert!(h.counter("mini_chat_attachment_upload").is_empty());
}

#[test]
fn only_bounded_labels_are_ever_attached() {
    let h = harness("");
    h.metrics
        .upload(AttachmentKind::Image, UploadResult::Failed);
    h.metrics.upload_bytes(AttachmentKind::Image, 10);
    h.metrics.pending_add(1);
    h.metrics.background_indexing(BackgroundResult::Timeout);
    h.metrics.upload_abandoned(AttachmentStatus::Uploaded);
    h.metrics.reaper_scan_seconds(1.0);
    h.metrics.cleanup_completed();
    h.metrics.cleanup_failed();
    h.metrics.cleanup_retry();
    h.metrics.secondary_cleanup_skipped();

    let allowed: BTreeSet<String> = [
        "kind",
        "result",
        "from_status",
        "resource_type",
        "reason",
        "provider_kind",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert!(
        !h.all_label_keys().is_empty(),
        "the instruments were recorded"
    );
    assert!(
        h.all_label_keys().is_subset(&allowed),
        "no tenant, user, chat, attachment, file or store id, no filename or MIME type: {:?}",
        h.all_label_keys()
    );
}

#[test]
fn every_attachment_instrument_is_exported_once_recorded() {
    let h = harness("");
    h.metrics.upload(AttachmentKind::Document, UploadResult::Ok);
    h.metrics.upload_bytes(AttachmentKind::Document, 1);
    h.metrics.pending_add(1);
    h.metrics.background_indexing(BackgroundResult::Ready);
    h.metrics.upload_abandoned(AttachmentStatus::Pending);
    h.metrics.reaper_scan_seconds(0.1);
    h.metrics.cleanup_completed();
    h.metrics.cleanup_failed();
    h.metrics.cleanup_retry();
    h.metrics.secondary_cleanup_skipped();

    let names = h.exported_names();

    for expected in [
        "mini_chat_attachment_upload",
        "mini_chat_attachment_upload_bytes",
        "mini_chat_attachments_pending",
        "mini_chat_attachment_background_indexing",
        "mini_chat_attachment_upload_abandoned",
        "mini_chat_upload_reaper_scan_duration_seconds",
        "mini_chat_cleanup_completed",
        "mini_chat_cleanup_failed",
        "mini_chat_cleanup_retry",
        "mini_chat_secondary_cleanup_skipped",
    ] {
        assert!(names.contains(expected), "{expected} in {names:?}");
    }
}
