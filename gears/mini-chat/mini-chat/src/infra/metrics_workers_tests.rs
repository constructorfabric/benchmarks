#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the worker instruments of DESIGN "Metric series"
//! (`cpt-cf-mini-chat-algo-background-workers-record-metrics`,
//! `cpt-cf-mini-chat-dod-background-workers-metrics`) recorded by
//! `MiniChatMetrics` through the `WorkerMetrics` port and the trigger metric of
//! the stream port, read back with the in-memory exporter: the exact instrument
//! names, the exact label values, no identifier or free-form label, and the
//! declared-but-unrecorded series.

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

use std::time::Duration;

use crate::domain::service::stream::ports::StreamMetrics;
use crate::domain::service::workers::metrics::{CleanupResource, SummaryResult, WorkerMetrics};
use crate::domain::service::workers::test_support::Metric;
use crate::test_doubles::USER;
use crate::worker_test_env::{WorkerEnv, WorkerOptions};

fn l(pairs: &[(&str, &str)]) -> Labels {
    label_set(pairs)
}

// ---- the orphan group -----------------------------------------------------------------------------------------------

#[test]
fn the_orphan_counters_carry_the_stale_progress_reason() {
    let h = harness("");
    h.metrics.orphan_detected();
    h.metrics.orphan_detected();
    h.metrics.orphan_finalized();

    assert_eq!(
        h.counter("mini_chat_orphan_detected"),
        vec![(l(&[("reason", "stale_progress")]), 2)]
    );
    assert_eq!(
        h.counter("mini_chat_orphan_finalized"),
        vec![(l(&[("reason", "stale_progress")]), 1)]
    );
}

#[test]
fn an_aborted_orphan_stream_counts_under_the_orphan_timeout_trigger_next_to_the_disconnect_one() {
    let h = harness("");
    h.metrics.orphan_stream_aborted();
    h.metrics.orphan_stream_aborted();
    StreamMetrics::streams_aborted(&h.metrics);

    assert_eq!(
        h.counter("mini_chat_streams_aborted"),
        vec![
            (l(&[("trigger", "client_disconnect")]), 1),
            (l(&[("trigger", "orphan_timeout")]), 2),
        ]
    );
}

#[test]
fn the_scan_duration_is_a_histogram_without_labels() {
    let h = harness("");
    h.metrics.orphan_scan_seconds(0.25);
    h.metrics.orphan_scan_seconds(0.75);

    assert_eq!(
        h.histogram("mini_chat_orphan_scan_duration_seconds"),
        vec![(Labels::new(), 2, 1.0)]
    );
}

// ---- the cleanup group ----------------------------------------------------------------------------------------------

#[test]
fn the_cleanup_counters_carry_the_resource_type() {
    let h = harness("");
    for _ in 0..4 {
        h.metrics.chat_cleanup_completed(CleanupResource::File);
    }
    h.metrics
        .chat_cleanup_completed(CleanupResource::VectorStore);
    h.metrics.chat_cleanup_failed(CleanupResource::File);
    h.metrics.chat_cleanup_failed(CleanupResource::VectorStore);

    assert_eq!(
        h.counter("mini_chat_cleanup_completed"),
        vec![
            (l(&[("resource_type", "file")]), 4),
            (l(&[("resource_type", "vector_store")]), 1),
        ]
    );
    assert_eq!(
        h.counter("mini_chat_cleanup_failed"),
        vec![
            (l(&[("resource_type", "file")]), 1),
            (l(&[("resource_type", "vector_store")]), 1),
        ]
    );
}

#[test]
fn the_cleanup_retry_counter_carries_the_resource_type_and_its_reason() {
    let h = harness("");
    h.metrics.chat_cleanup_retry(CleanupResource::File);
    h.metrics.chat_cleanup_retry(CleanupResource::File);
    h.metrics.chat_cleanup_retry(CleanupResource::VectorStore);

    assert_eq!(
        h.counter("mini_chat_cleanup_retry"),
        vec![
            (
                l(&[("reason", "provider_error"), ("resource_type", "file")]),
                2
            ),
            (
                l(&[
                    ("reason", "vector_store_delete_failed"),
                    ("resource_type", "vector_store")
                ]),
                1
            ),
        ]
    );
}

#[test]
fn the_failed_attachment_debt_and_the_skipped_secondary_counters() {
    let h = harness("");
    h.metrics.chat_cleanup_store_with_failed_attachments();
    h.metrics.chat_cleanup_store_with_failed_attachments();
    h.metrics.chat_secondary_cleanup_skipped();

    assert_eq!(
        h.counter("mini_chat_cleanup_vector_store_with_failed_attachments"),
        vec![(Labels::new(), 2)]
    );
    assert_eq!(
        h.counter("mini_chat_secondary_cleanup_skipped"),
        vec![(l(&[("provider_kind", "anthropic")]), 1)]
    );
}

// ---- the thread summary group -------------------------------------------------------------------------------------

#[test]
fn the_summary_trigger_counter_carries_scheduled_and_not_needed() {
    let h = harness("");
    StreamMetrics::thread_summary_trigger(&h.metrics, "scheduled");
    StreamMetrics::thread_summary_trigger(&h.metrics, "scheduled");
    StreamMetrics::thread_summary_trigger(&h.metrics, "not_needed");

    assert_eq!(
        h.counter("mini_chat_thread_summary_trigger"),
        vec![
            (l(&[("result", "not_needed")]), 1),
            (l(&[("result", "scheduled")]), 2),
        ]
    );
}

#[test]
fn the_summary_execution_counter_carries_every_documented_result() {
    let h = harness("");
    let results = [
        (SummaryResult::Success, "success"),
        (SummaryResult::ProviderError, "provider_error"),
        (SummaryResult::EmptySummary, "empty_summary"),
        (SummaryResult::Retry, "retry"),
        (SummaryResult::ModelUnavailable, "model_unavailable"),
        (SummaryResult::FrontierDeleted, "frontier_deleted"),
        (SummaryResult::BaseMissing, "base_missing"),
    ];
    for (result, _) in results {
        h.metrics.summary_execution(result);
    }

    let mut expected: Vec<(Labels, u64)> = results
        .iter()
        .map(|(_, name)| (l(&[("result", name)]), 1))
        .collect();
    expected.sort();
    assert_eq!(h.counter("mini_chat_thread_summary_execution"), expected);
}

#[test]
fn the_cas_conflict_and_fallback_counters_have_no_labels() {
    let h = harness("");
    h.metrics.summary_cas_conflict();
    h.metrics.summary_cas_conflict();
    h.metrics.summary_fallback();

    assert_eq!(
        h.counter("mini_chat_thread_summary_cas_conflicts"),
        vec![(Labels::new(), 2)]
    );
    assert_eq!(
        h.counter("mini_chat_summary_fallback"),
        vec![(Labels::new(), 1)]
    );
}

// ---- the rules ----------------------------------------------------------------------------------------------------

fn record_everything(h: &Harness) {
    let m = &h.metrics;
    m.orphan_detected();
    m.orphan_finalized();
    m.orphan_stream_aborted();
    m.orphan_scan_seconds(0.1);
    for resource in [CleanupResource::File, CleanupResource::VectorStore] {
        m.chat_cleanup_completed(resource);
        m.chat_cleanup_failed(resource);
        m.chat_cleanup_retry(resource);
    }
    m.chat_cleanup_store_with_failed_attachments();
    m.chat_secondary_cleanup_skipped();
    StreamMetrics::thread_summary_trigger(m, "scheduled");
    m.summary_execution(SummaryResult::Success);
    m.summary_cas_conflict();
    m.summary_fallback();
}

#[test]
fn only_the_bounded_labels_of_the_worker_groups_exist() {
    let h = harness("");
    record_everything(&h);

    let used = h.all_label_keys();

    let allowed: BTreeSet<String> = [
        "reason",
        "resource_type",
        "trigger",
        "provider_kind",
        "result",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert!(used.is_subset(&allowed), "unexpected label keys: {used:?}");
    for forbidden in [
        "tenant_id",
        "user_id",
        "chat_id",
        "turn_id",
        "request_id",
        "attachment_id",
        "file_id",
        "vector_store_id",
        "prompt",
        "error",
    ] {
        assert!(!used.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn the_declared_but_unrecorded_series_stay_silent() {
    let h = harness("");
    record_everything(&h);

    let exported = h.exported_names();

    for silent in [
        "cleanup_backlog",
        "summary_regen",
        "outbox_dead",
        "outbox_dead_rows",
        "outbox_enqueue",
    ] {
        assert!(
            !exported.contains(&format!("mini_chat_{silent}")),
            "`{silent}` must never be recorded"
        );
    }
}

#[test]
fn every_worker_instrument_is_exported_under_its_documented_name() {
    let h = harness("");
    record_everything(&h);

    let exported = h.exported_names();

    for name in [
        "orphan_detected",
        "orphan_finalized",
        "orphan_scan_duration_seconds",
        "streams_aborted",
        "cleanup_completed",
        "cleanup_failed",
        "cleanup_retry",
        "cleanup_vector_store_with_failed_attachments",
        "secondary_cleanup_skipped",
        "thread_summary_trigger",
        "thread_summary_execution",
        "thread_summary_cas_conflicts",
        "summary_fallback",
    ] {
        assert!(
            exported.contains(&format!("mini_chat_{name}")),
            "{name}: {exported:?}"
        );
    }
}

#[test]
fn the_worker_instrument_names_follow_the_configured_prefix() {
    let h = harness("shop");
    h.metrics.orphan_finalized();
    h.metrics.summary_cas_conflict();

    assert_eq!(h.counter("shop_orphan_finalized").len(), 1);
    assert_eq!(h.counter("shop_thread_summary_cas_conflicts").len(), 1);
    assert!(h.counter("mini_chat_orphan_finalized").is_empty());
}

#[test]
fn the_cleanup_instruments_are_shared_with_the_attachment_cleanup_handler() {
    use crate::domain::service::attachments::ports::AttachmentMetrics;
    let h = harness("");
    AttachmentMetrics::cleanup_completed(&h.metrics);
    h.metrics.chat_cleanup_completed(CleanupResource::File);

    assert_eq!(
        h.counter("mini_chat_cleanup_completed"),
        vec![(l(&[("resource_type", "file")]), 2)],
        "one instrument, one series"
    );
}

// ---- the scenarios ------------------------------------------------------------------------------------------------

/// Polls the real instruments until `check` holds (10 s at most): a metric is
/// never asserted at the very instant the state it follows was observed.
async fn eventually(env: &WorkerEnv, what: &str, check: impl Fn(&Harness) -> bool) {
    let otel = env.otel.as_ref().expect("real metrics");
    let h = Harness {
        provider: otel.provider.clone(),
        exporter: otel.exporter.clone(),
        // the harness only reads; its own instruments live under another prefix
        metrics: MiniChatMetrics::new(&otel.provider.meter("unused"), "unused"),
    };
    for _ in 0..1000 {
        if check(&h) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn real_env() -> WorkerOptions {
    WorkerOptions {
        real_metrics: true,
        ..WorkerOptions::default()
    }
}

#[tokio::test]
async fn an_orphan_finalization_records_the_orphan_series_with_exact_labels() {
    use crate::infra::workers::leader::{LeaderElector, NoopLeaderElector};
    use crate::quota_test_env::TurnSeed;
    let env = WorkerEnv::with(real_env()).await;
    env.quota.seed_design_world(1_500_000, false).await;
    env.quota
        .seed_turn(
            &TurnSeed::standard(1, env.chat)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:10:00Z")),
        )
        .await;
    let elector: std::sync::Arc<dyn LeaderElector> = std::sync::Arc::new(NoopLeaderElector);

    env.watchdog(elector).scan_once().await;

    eventually(&env, "the orphan series", |h| {
        h.counter("mini_chat_orphan_detected") == vec![(l(&[("reason", "stale_progress")]), 1)]
            && h.counter("mini_chat_orphan_finalized")
                == vec![(l(&[("reason", "stale_progress")]), 1)]
            && h.counter("mini_chat_streams_aborted")
                == vec![(l(&[("trigger", "orphan_timeout")]), 1)]
            && h.histogram("mini_chat_orphan_scan_duration_seconds")
                .iter()
                .map(|p| p.1)
                .sum::<u64>()
                == 1
    })
    .await;
    assert_eq!(env.metrics.count(Metric::OrphanFinalized), 1);
    let _ = USER;
}

#[tokio::test]
async fn a_chat_deletion_records_the_cleanup_series_with_exact_labels() {
    let env = WorkerEnv::with(real_env()).await;
    env.script_happy();
    env.oagw.on_path_sequence(
        "DELETE",
        "/api.openai.test/v1/files/file-prov2",
        vec![(500, serde_json::json!({})), (200, serde_json::json!({}))],
    );
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    for n in 1..=2 {
        env.owed(chat, n).await;
    }
    env.store_row(chat, Some("vs_chat"), "openai").await;
    env.soft_delete_chat(chat).await;

    env.deliver_cleanup(chat, 0).await;
    env.deliver_cleanup(chat, 1).await;

    eventually(&env, "the cleanup series", |h| {
        h.counter("mini_chat_cleanup_completed")
            == vec![
                (l(&[("resource_type", "file")]), 2),
                (l(&[("resource_type", "vector_store")]), 1),
            ]
            && h.counter("mini_chat_cleanup_retry")
                == vec![(
                    l(&[("reason", "provider_error"), ("resource_type", "file")]),
                    1,
                )]
    })
    .await;
}

#[tokio::test]
async fn a_summary_failure_and_a_commit_record_the_summary_series_with_exact_labels() {
    use crate::test_doubles::ProxyReply;
    let env = WorkerEnv::with(real_env()).await;
    let turns = env.conversation(3).await;
    let task = WorkerEnv::task(
        env.chat,
        None,
        crate::worker_test_env::boundary(turns[2].assistant_message, 31),
    );
    env.oagw
        .push_proxy(ProxyReply::json(500, &serde_json::json!({})));
    env.oagw.push_proxy(ProxyReply::json(
        200,
        &serde_json::json!({
            "output": [ { "type": "message", "content": [ { "type": "output_text", "text": "<summary>S</summary>" } ] } ],
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }),
    ));

    env.deliver_summary(&task, 0).await;
    env.deliver_summary(&task, 1).await;
    env.deliver_summary(&task, 2).await;

    eventually(&env, "the summary series", |h| {
        h.counter("mini_chat_thread_summary_execution")
            == vec![
                (l(&[("result", "provider_error")]), 1),
                (l(&[("result", "success")]), 1),
            ]
            && h.counter("mini_chat_summary_fallback") == vec![(Labels::new(), 1)]
            && h.counter("mini_chat_thread_summary_cas_conflicts") == vec![(Labels::new(), 1)]
    })
    .await;
}
