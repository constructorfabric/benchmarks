//! The reaper, cleanup and knowledge-search series on the real OTLP instruments: names, kinds and
//! labels of `mini_chat_attachment_upload_abandoned{from_status}`,
//! `mini_chat_upload_reaper_scan_duration_seconds`, `mini_chat_cleanup_completed`,
//! `mini_chat_cleanup_retry`, `mini_chat_cleanup_failed` (each with `resource_type="file"`),
//! `mini_chat_secondary_cleanup_skipped{provider_kind}`, `mini_chat_knowledge_search{result}`,
//! `mini_chat_knowledge_search_latency_ms` and `mini_chat_knowledge_search_chunks`; low cardinality
//! only; the declared-but-unrecorded series stay unrecorded; and the series recorded by the real
//! reaper and the real cleanup handler.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics` (`inst-met-reaper`,
//! `inst-met-cleanup`, `inst-met-knowledge`, `inst-met-deferred`, `inst-met-labels`,
//! `inst-met-return`), flow `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability`
//! (`inst-adm-read-metrics`, `inst-adm-cleanup-failed`, `inst-adm-return`); definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use toolkit_db::outbox::LeasedMessageHandler;

use crate::domain::attachments::AttachmentMetrics;
use crate::test_support::attachments::{
    AttachRigOptions, RowSeed, cleanup_message, deleted_payload, rig_with, seed_row,
};
use crate::test_support::quota::default_now;
use crate::test_support::s2::{MetricsHarness, Point, metrics_harness};
use crate::test_support::storage_mock::{Answer, Route};

fn harness() -> MetricsHarness {
    metrics_harness("mini_chat")
}

fn points(harness: &MetricsHarness, name: &str) -> Vec<Point> {
    harness
        .all_points()
        .into_iter()
        .filter(|(point_name, _, _)| point_name == name)
        .collect()
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn single(harness: &MetricsHarness, name: &str) -> (BTreeMap<String, String>, i64) {
    let found = points(harness, name);
    assert_eq!(found.len(), 1, "{name}: {found:?}");
    (found[0].1.clone(), found[0].2)
}

// ── the reaper series ────────────────────────────────────────────────────────

#[test]
fn the_abandoned_counter_is_labelled_by_from_status_only() {
    // inst-met-reaper
    let harness = harness();
    harness.metrics.upload_abandoned("pending");
    harness.metrics.upload_abandoned("uploaded");
    harness.metrics.upload_abandoned("uploaded");
    let found = points(&harness, "mini_chat_attachment_upload_abandoned");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.contains(&(
        "mini_chat_attachment_upload_abandoned".to_owned(),
        labels(&[("from_status", "pending")]),
        1
    )));
    assert!(found.contains(&(
        "mini_chat_attachment_upload_abandoned".to_owned(),
        labels(&[("from_status", "uploaded")]),
        2
    )));
}

#[test]
fn the_scan_duration_is_a_histogram_without_labels() {
    // inst-met-reaper: `mini_chat_upload_reaper_scan_duration_seconds` per scan
    let harness = harness();
    harness
        .metrics
        .reaper_scan_duration(Duration::from_millis(250));
    harness.metrics.reaper_scan_duration(Duration::from_secs(2));
    let (attributes, count) = single(&harness, "mini_chat_upload_reaper_scan_duration_seconds");
    assert!(attributes.is_empty(), "{attributes:?}");
    assert_eq!(count, 2, "two samples");
}

// ── the cleanup series ───────────────────────────────────────────────────────

#[test]
fn the_cleanup_counters_carry_the_file_resource_type() {
    // inst-met-cleanup
    let harness = harness();
    harness.metrics.cleanup_completed();
    harness.metrics.cleanup_completed();
    harness.metrics.cleanup_retry("provider_error");
    harness.metrics.cleanup_failed();
    assert_eq!(
        single(&harness, "mini_chat_cleanup_completed"),
        (labels(&[("resource_type", "file")]), 2)
    );
    assert_eq!(
        single(&harness, "mini_chat_cleanup_retry"),
        (
            labels(&[("resource_type", "file"), ("reason", "provider_error")]),
            1
        )
    );
    assert_eq!(
        single(&harness, "mini_chat_cleanup_failed"),
        (labels(&[("resource_type", "file")]), 1)
    );
}

#[test]
fn the_skipped_secondary_cleanup_is_labelled_by_provider_kind() {
    // inst-met-cleanup: `mini_chat_secondary_cleanup_skipped{provider_kind}`
    let harness = harness();
    harness.metrics.secondary_cleanup_skipped("anthropic");
    assert_eq!(
        single(&harness, "mini_chat_secondary_cleanup_skipped"),
        (labels(&[("provider_kind", "anthropic")]), 1)
    );
}

// ── the knowledge series ─────────────────────────────────────────────────────

#[test]
fn the_knowledge_series_are_labelled_by_result_and_unlabelled() {
    // inst-met-knowledge
    let harness = harness();
    harness.metrics.knowledge_search("ok");
    harness.metrics.knowledge_search("ok");
    harness.metrics.knowledge_search("error");
    harness
        .metrics
        .knowledge_search_latency(Duration::from_millis(120));
    harness.metrics.knowledge_search_chunks(3);
    harness.metrics.knowledge_search_chunks(0);
    let found = points(&harness, "mini_chat_knowledge_search");
    assert!(found.contains(&(
        "mini_chat_knowledge_search".to_owned(),
        labels(&[("result", "ok")]),
        2
    )));
    assert!(found.contains(&(
        "mini_chat_knowledge_search".to_owned(),
        labels(&[("result", "error")]),
        1
    )));
    let (latency_labels, latency_count) = single(&harness, "mini_chat_knowledge_search_latency_ms");
    assert!(latency_labels.is_empty());
    assert_eq!(latency_count, 1);
    let (chunk_labels, chunk_count) = single(&harness, "mini_chat_knowledge_search_chunks");
    assert!(chunk_labels.is_empty());
    assert_eq!(chunk_count, 2);
}

// ── labels and deferred series ───────────────────────────────────────────────

fn record_everything(metrics: &dyn AttachmentMetrics) {
    metrics.upload_abandoned("pending");
    metrics.reaper_scan_duration(Duration::from_millis(5));
    metrics.cleanup_completed();
    metrics.cleanup_retry("provider_error");
    metrics.cleanup_failed();
    metrics.secondary_cleanup_skipped("anthropic");
    metrics.knowledge_search("ok");
    metrics.knowledge_search_latency(Duration::from_millis(5));
    metrics.knowledge_search_chunks(1);
}

#[test]
fn no_label_of_these_series_carries_an_identifier() {
    // inst-met-labels: low-cardinality labels only
    let harness = harness();
    record_everything(&*harness.metrics);
    let allowed = [
        "from_status",
        "resource_type",
        "reason",
        "provider_kind",
        "result",
    ];
    for (name, attributes, _) in harness.all_points() {
        let relevant = [
            "upload_abandoned",
            "upload_reaper_scan",
            "cleanup_",
            "secondary_cleanup",
            "knowledge_search",
        ]
        .iter()
        .any(|part| name.contains(part));
        if !relevant {
            continue;
        }
        for key in attributes.keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "{name} carries the label `{key}`"
            );
        }
    }
}

#[test]
fn the_declared_but_unrecorded_series_stay_unrecorded() {
    // inst-met-deferred, inst-met-return
    let harness = harness();
    record_everything(&*harness.metrics);
    let recorded: Vec<String> = harness
        .all_points()
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    for deferred in [
        "file_search_latency_ms",
        "retrieval_latency_ms",
        "retrieval_chunks_returned",
        "retrieval_zero_hit",
        "indexed_chunks_per_chat",
        "upload_rejected",
        "vector_stores_per_user",
        "attachment_index",
        "attachment_summary",
        "attachments_failed",
        "attachment_index_latency_ms",
        "media_rejected",
        "cleanup_backlog",
    ] {
        let full = format!("mini_chat_{deferred}");
        assert!(!recorded.contains(&full), "`{full}` was recorded");
    }
}

#[test]
fn the_series_are_registered_instruments() {
    let harness = harness();
    let names = harness.metrics.instrument_names();
    for name in [
        "mini_chat_attachment_upload_abandoned",
        "mini_chat_upload_reaper_scan_duration_seconds",
        "mini_chat_cleanup_completed",
        "mini_chat_cleanup_retry",
        "mini_chat_cleanup_failed",
        "mini_chat_secondary_cleanup_skipped",
        "mini_chat_knowledge_search",
        "mini_chat_knowledge_search_latency_ms",
        "mini_chat_knowledge_search_chunks",
    ] {
        assert!(
            names.contains(&name.to_owned()),
            "`{name}` is not registered"
        );
    }
}

#[test]
fn the_series_follow_the_configured_prefix() {
    let harness = metrics_harness("custom_prefix");
    harness.metrics.cleanup_completed();
    harness.metrics.upload_abandoned("uploaded");
    assert_eq!(points(&harness, "custom_prefix_cleanup_completed").len(), 1);
    assert_eq!(
        points(&harness, "custom_prefix_attachment_upload_abandoned").len(),
        1
    );
    assert!(points(&harness, "mini_chat_cleanup_completed").is_empty());
}

// ── recorded by the real reaper and the real handler ─────────────────────────

#[tokio::test]
async fn the_real_reaper_records_the_abandoned_and_duration_series() {
    // inst-rp-commit, inst-rp-duration over the OTLP pipeline
    let rig = rig_with(AttachRigOptions::new().with_otlp()).await;
    let chat = rig.std_chat().await;
    let stale = default_now() - time::Duration::seconds(900);
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("uploaded").updated(stale),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("pending")
            .no_provider_file()
            .updated(stale),
    )
    .await;
    rig.reaper().scan_once().await.unwrap();
    let otlp = rig.otlp.as_ref().unwrap();
    assert_eq!(
        otlp.total_where("attachment_upload_abandoned", "from_status", "uploaded"),
        1
    );
    assert_eq!(
        otlp.total_where("attachment_upload_abandoned", "from_status", "pending"),
        1
    );
    assert_eq!(otlp.total("upload_reaper_scan_duration_seconds"), 1);
    assert_eq!(
        otlp.total("attachments_pending"),
        -1,
        "the pending row left `pending`"
    );
}

#[tokio::test]
async fn the_real_handler_records_the_completion_retry_and_failure_series() {
    // inst-ch-mark-done, inst-ch-return-retry, inst-ch-exhausted over the OTLP pipeline
    let rig = rig_with(AttachRigOptions::new().with_otlp()).await;
    let chat = rig.std_chat().await;
    let ok = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    let failing = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    let cleanup = rig.cleanup_with(2, true);
    cleanup
        .handler
        .handle(&cleanup_message(
            &deleted_payload(&chat, ok, Some("file-ok")),
            0,
        ))
        .await;
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let failing_message = cleanup_message(&deleted_payload(&chat, failing, Some("file-bad")), 0);
    cleanup.handler.handle(&failing_message).await;
    cleanup.handler.handle(&failing_message).await;
    let otlp = rig.otlp.as_ref().unwrap();
    assert_eq!(
        otlp.total_where("cleanup_completed", "resource_type", "file"),
        1
    );
    assert_eq!(
        otlp.total_where("cleanup_retry", "reason", "provider_error"),
        1
    );
    assert_eq!(
        otlp.total_where("cleanup_failed", "resource_type", "file"),
        1
    );
}
