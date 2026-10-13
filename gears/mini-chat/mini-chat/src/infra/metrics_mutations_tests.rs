#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The mutation instruments
//! (`cpt-cf-mini-chat-algo-turn-mutations-record-metrics`) recorded by
//! `MiniChatMetrics` through the `MutationMetrics` port, read back with the
//! in-memory exporter.

use std::collections::BTreeMap;

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::MiniChatMetrics;
use crate::domain::service::mutations::ports::MutationMetrics;
use crate::domain::service::mutations::types::{MutationOp, MutationResult};

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
    fn counter(&self, name: &str) -> Vec<(Labels, u64)> {
        self.provider.force_flush().expect("flush");
        let finished = self.exporter.get_finished_metrics().unwrap();
        let mut out = Vec::new();
        for rm in finished.iter().rev().take(1) {
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

    fn histogram(&self, name: &str) -> Vec<(Labels, u64, f64)> {
        self.provider.force_flush().expect("flush");
        let finished = self.exporter.get_finished_metrics().unwrap();
        let mut out = Vec::new();
        for rm in finished.iter().rev().take(1) {
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
}

#[test]
fn every_result_is_counted_with_exactly_the_op_and_result_labels() {
    let h = harness("");
    h.metrics
        .turn_mutation(MutationOp::Retry, MutationResult::Ok);
    h.metrics
        .turn_mutation(MutationOp::Retry, MutationResult::Ok);
    h.metrics
        .turn_mutation(MutationOp::Delete, MutationResult::NotLatestTurn);
    h.metrics
        .turn_mutation(MutationOp::Edit, MutationResult::SetupFailed);

    let points = h.counter("mini_chat_turn_mutation");

    assert_eq!(
        points,
        vec![
            (
                label_set(&[("op", "delete"), ("result", "not_latest_turn")]),
                1
            ),
            (label_set(&[("op", "edit"), ("result", "setup_failed")]), 1),
            (label_set(&[("op", "retry"), ("result", "ok")]), 2),
        ]
    );
}

#[test]
fn the_latency_histogram_carries_only_the_op_label() {
    let h = harness("");
    h.metrics.turn_mutation_latency_ms(MutationOp::Retry, 12.5);
    h.metrics.turn_mutation_latency_ms(MutationOp::Retry, 7.5);
    h.metrics.turn_mutation_latency_ms(MutationOp::Edit, 3.0);

    let mut points = h.histogram("mini_chat_turn_mutation_latency_ms");
    points.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(
        points,
        vec![
            (label_set(&[("op", "edit")]), 1, 3.0),
            (label_set(&[("op", "retry")]), 2, 20.0),
        ]
    );
}

#[test]
fn the_configured_prefix_names_the_instruments() {
    let h = harness("chat");
    h.metrics
        .turn_mutation(MutationOp::Edit, MutationResult::Ok);
    h.metrics.turn_mutation_latency_ms(MutationOp::Edit, 1.0);

    assert_eq!(h.counter("chat_turn_mutation").len(), 1);
    assert_eq!(h.histogram("chat_turn_mutation_latency_ms").len(), 1);
    assert!(h.counter("mini_chat_turn_mutation").is_empty());
}

#[test]
fn no_label_carries_a_tenant_user_chat_or_request_id() {
    let h = harness("");
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        for result in [
            MutationResult::Ok,
            MutationResult::NotLatestTurn,
            MutationResult::GenerationInProgress,
            MutationResult::NotTerminal,
            MutationResult::Rejected,
            MutationResult::SetupFailed,
            MutationResult::Error,
        ] {
            h.metrics.turn_mutation(op, result);
        }
        h.metrics.turn_mutation_latency_ms(op, 1.0);
    }

    let counters = h.counter("mini_chat_turn_mutation");
    let histograms = h.histogram("mini_chat_turn_mutation_latency_ms");

    assert_eq!(counters.len(), 21);
    for (labels, _) in &counters {
        assert_eq!(
            labels.keys().map(String::as_str).collect::<Vec<_>>(),
            ["op", "result"]
        );
    }
    for (labels, _, _) in &histograms {
        assert_eq!(
            labels.keys().map(String::as_str).collect::<Vec<_>>(),
            ["op"]
        );
    }
}
