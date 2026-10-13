#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the request group of the cost metrics
//! (`cpt-cf-mini-chat-algo-streaming-turns-record-metrics`) recorded by
//! `MiniChatMetrics` through the `StreamMetrics` port, read back with the
//! in-memory exporter.

use std::collections::{BTreeMap, BTreeSet};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::MiniChatMetrics;
use crate::domain::service::stream::ports::{DisconnectStage, StreamMetrics};

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

fn provider_model() -> Labels {
    label_set(&[("model", "gpt-5.2"), ("provider", "openai")])
}

// ---- the streaming counters ---------------------------------------------------------------------

#[test]
fn stream_started_carries_provider_and_model() {
    let h = harness("");
    h.metrics.stream_started("openai", "gpt-5.2");
    h.metrics.stream_started("openai", "gpt-5.2");
    assert_eq!(
        h.counter("mini_chat_stream_started"),
        vec![(provider_model(), 2)]
    );
}

#[test]
fn the_active_streams_gauge_goes_up_and_down() {
    let h = harness("");
    h.metrics.active_streams_add(1);
    h.metrics.active_streams_add(1);
    h.metrics.active_streams_add(-1);
    assert_eq!(
        h.up_down("mini_chat_active_streams"),
        vec![(Labels::new(), 1)]
    );
    h.metrics.active_streams_add(-1);
    assert_eq!(
        h.up_down("mini_chat_active_streams"),
        vec![(Labels::new(), 0)]
    );
}

#[test]
fn completion_incompletion_and_failure_counters_have_their_labels() {
    let h = harness("");
    h.metrics.stream_completed("openai", "gpt-5.2");
    h.metrics
        .stream_incomplete("openai", "gpt-5.2", "max_output_tokens");
    h.metrics
        .stream_failed("openai", "gpt-5.2", "provider_timeout");
    assert_eq!(
        h.counter("mini_chat_stream_completed"),
        vec![(provider_model(), 1)]
    );
    assert_eq!(
        h.counter("mini_chat_stream_incomplete"),
        vec![(
            label_set(&[
                ("model", "gpt-5.2"),
                ("provider", "openai"),
                ("reason", "max_output_tokens")
            ]),
            1
        )]
    );
    assert_eq!(
        h.counter("mini_chat_stream_failed"),
        vec![(
            label_set(&[
                ("error_code", "provider_timeout"),
                ("model", "gpt-5.2"),
                ("provider", "openai")
            ]),
            1
        )]
    );
}

#[test]
fn the_latency_histograms_carry_provider_and_model() {
    let h = harness("");
    h.metrics.ttft_provider_ms("openai", "gpt-5.2", 120.0);
    h.metrics.ttft_overhead_ms("openai", "gpt-5.2", 3.0);
    h.metrics
        .stream_total_latency_ms("openai", "gpt-5.2", 2500.0);
    for (name, sum) in [
        ("mini_chat_ttft_provider_ms", 120.0),
        ("mini_chat_ttft_overhead_ms", 3.0),
        ("mini_chat_stream_total_latency_ms", 2500.0),
    ] {
        assert_eq!(
            h.histogram(name),
            vec![(provider_model(), 1, sum)],
            "{name}"
        );
    }
}

// ---- cancellation -------------------------------------------------------------------------------

#[test]
fn the_cancellation_instruments_use_the_disconnect_trigger() {
    let h = harness("");
    h.metrics.cancel_requested();
    h.metrics.cancel_effective();
    h.metrics.streams_aborted();
    h.metrics.time_to_abort_ms(42.0);
    assert_eq!(
        h.counter("mini_chat_cancel_requested"),
        vec![(label_set(&[("trigger", "disconnect")]), 1)]
    );
    assert_eq!(
        h.counter("mini_chat_cancel_effective"),
        vec![(label_set(&[("trigger", "disconnect")]), 1)]
    );
    assert_eq!(
        h.counter("mini_chat_streams_aborted"),
        vec![(label_set(&[("trigger", "client_disconnect")]), 1)]
    );
    assert_eq!(
        h.histogram("mini_chat_time_to_abort_ms"),
        vec![(label_set(&[("trigger", "disconnect")]), 1, 42.0)]
    );
}

#[test]
fn the_disconnect_counter_carries_the_stage() {
    let h = harness("");
    h.metrics
        .stream_disconnected(DisconnectStage::BeforeFirstToken);
    h.metrics.stream_disconnected(DisconnectStage::MidStream);
    h.metrics.stream_disconnected(DisconnectStage::MidStream);
    assert_eq!(
        h.counter("mini_chat_stream_disconnected"),
        vec![
            (label_set(&[("stage", "before_first_token")]), 1),
            (label_set(&[("stage", "mid_stream")]), 2),
        ]
    );
    assert_eq!(
        DisconnectStage::BeforeFirstToken.as_str(),
        "before_first_token"
    );
    assert_eq!(DisconnectStage::MidStream.as_str(), "mid_stream");
}

// ---- turn and knowledge instruments -------------------------------------------------------------

#[test]
fn the_image_count_and_the_finalization_latency_have_no_labels() {
    let h = harness("");
    h.metrics.image_inputs_per_turn(3);
    h.metrics.finalization_latency_ms(17.0);
    assert_eq!(
        h.histogram("mini_chat_image_inputs_per_turn"),
        vec![(Labels::new(), 1, 3.0)]
    );
    assert_eq!(
        h.histogram("mini_chat_finalization_latency_ms"),
        vec![(Labels::new(), 1, 17.0)]
    );
}

#[test]
fn the_knowledge_search_instruments_carry_the_result_only() {
    let h = harness("");
    h.metrics.knowledge_search("ok");
    h.metrics.knowledge_search("error");
    h.metrics.knowledge_search("ok");
    h.metrics.knowledge_search_latency_ms(80.0);
    h.metrics.knowledge_search_chunks(4);
    assert_eq!(
        h.counter("mini_chat_knowledge_search"),
        vec![
            (label_set(&[("result", "error")]), 1),
            (label_set(&[("result", "ok")]), 2),
        ]
    );
    assert_eq!(
        h.histogram("mini_chat_knowledge_search_latency_ms"),
        vec![(Labels::new(), 1, 80.0)]
    );
    assert_eq!(
        h.histogram("mini_chat_knowledge_search_chunks"),
        vec![(Labels::new(), 1, 4.0)]
    );
}

// ---- rules --------------------------------------------------------------------------------------

fn record_everything(h: &Harness) {
    let m = &h.metrics;
    m.stream_started("openai", "gpt-5.2");
    m.active_streams_add(1);
    m.ttft_provider_ms("openai", "gpt-5.2", 1.0);
    m.ttft_overhead_ms("openai", "gpt-5.2", 1.0);
    m.stream_completed("openai", "gpt-5.2");
    m.stream_incomplete("openai", "gpt-5.2", "max_output_tokens");
    m.stream_failed("openai", "gpt-5.2", "provider_error");
    m.stream_total_latency_ms("openai", "gpt-5.2", 1.0);
    m.cancel_requested();
    m.stream_disconnected(DisconnectStage::MidStream);
    m.time_to_abort_ms(1.0);
    m.cancel_effective();
    m.streams_aborted();
    m.image_inputs_per_turn(1);
    m.finalization_latency_ms(1.0);
    m.knowledge_search("ok");
    m.knowledge_search_latency_ms(1.0);
    m.knowledge_search_chunks(1);
}

#[test]
fn only_the_low_cardinality_labels_exist() {
    let h = harness("");
    record_everything(&h);
    let allowed: BTreeSet<String> = [
        "provider",
        "model",
        "error_code",
        "reason",
        "stage",
        "trigger",
        "result",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let used = h.all_label_keys();
    assert!(used.is_subset(&allowed), "unexpected label keys: {used:?}");
    for forbidden in [
        "tenant_id",
        "user_id",
        "chat_id",
        "request_id",
        "provider_response_id",
    ] {
        assert!(!used.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn the_declared_but_unrecorded_instruments_stay_silent() {
    let h = harness("");
    record_everything(&h);
    let exported = h.exported_names();
    for silent in [
        "stream_replay",
        "tool_calls",
        "tool_call_limited",
        "citations_count",
        "citations_by_source",
        "context_truncation",
        "provider_requests",
        "provider_errors",
        "provider_latency_ms",
        "tokens_after_cancel",
        "time_from_ui_disconnect_to_cancel_ms",
        "cancel_orphan",
        "web_search_disabled",
        "unknown_error_code",
    ] {
        assert!(
            !exported.contains(&format!("mini_chat_{silent}")),
            "`{silent}` must never be recorded"
        );
    }
}

#[test]
fn the_instrument_names_follow_the_configured_prefix() {
    let h = harness("shop");
    h.metrics.stream_started("openai", "gpt-5.2");
    h.metrics.image_inputs_per_turn(1);
    assert_eq!(h.counter("shop_stream_started").len(), 1);
    assert_eq!(h.histogram("shop_image_inputs_per_turn").len(), 1);
    assert!(h.counter("mini_chat_stream_started").is_empty());
}
