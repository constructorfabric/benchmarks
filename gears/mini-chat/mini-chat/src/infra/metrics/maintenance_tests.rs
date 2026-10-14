//! The background-maintenance series on the real OTLP instruments: names, kinds and labels of
//! `mini_chat_thread_summary_execution{result}` (every allowlist value),
//! `mini_chat_thread_summary_cas_conflicts`, `mini_chat_summary_fallback`,
//! `mini_chat_cleanup_completed{resource_type}`, `mini_chat_cleanup_retry{resource_type, reason}`,
//! `mini_chat_cleanup_failed{resource_type}`,
//! `mini_chat_cleanup_vector_store_with_failed_attachments` and
//! `mini_chat_secondary_cleanup_skipped{provider_kind}`; low cardinality only; the file series
//! shared with the attachment cleanup handler; the series recorded by the real summary worker and
//! the real chat-cleanup worker; and the series that stay declared but unrecorded (`cleanup_backlog`,
//! `summary_regen`, the `outbox_*` series, `thread_summary_trigger`).
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-metrics` (`inst-mt-execution`, `inst-mt-cas`,
//! `inst-mt-fallback`, `inst-mt-cleanup`, `inst-mt-deferred`, `inst-mt-trigger-elsewhere`,
//! `inst-mt-labels`, `inst-mt-return`); flow
//! `cpt-cf-mini-chat-flow-background-maintenance-admin-observability` (`inst-adm-read-metrics`,
//! `inst-adm-summary-alert`, `inst-adm-store-alert`, `inst-adm-file-debt`,
//! `inst-adm-outbox-surface`, `inst-adm-return`); definition of done
//! `cpt-cf-mini-chat-dod-background-maintenance-observability-config`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use serde_json::json;

use crate::domain::attachments::AttachmentMetrics;
use crate::domain::maintenance::metrics::{CleanupMetrics, SummaryMetrics};
use crate::infra::metrics::Instrument;
use crate::test_support::attachments::{RowSeed, seed_row};
use crate::test_support::maintenance::{
    MaintOptions, SUMMARY_MODEL, chat_cleanup_payload, maint_rig_with, responses_answer,
    seed_range, seed_store, settings_for, summary_payload, summary_text,
};
use crate::test_support::provider::Reply;
use crate::test_support::s2::{MetricsHarness, metrics_harness};
use crate::test_support::send::t;

fn harness() -> MetricsHarness {
    metrics_harness("mini_chat")
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn series(harness: &MetricsHarness, name: &str) -> Vec<(BTreeMap<String, String>, i64)> {
    harness.series(name)
}

// ── the summary series ───────────────────────────────────────────────────────

#[test]
fn the_execution_counter_carries_the_result_label_only() {
    // inst-mt-execution: exactly the seven allowlist values, one label
    let harness = harness();
    for result in [
        "success",
        "provider_error",
        "empty_summary",
        "retry",
        "model_unavailable",
        "frontier_deleted",
        "base_missing",
    ] {
        harness.metrics.execution(result);
    }
    harness.metrics.execution("success");
    let found = series(&harness, "thread_summary_execution");
    assert_eq!(found.len(), 7, "{found:?}");
    for (attributes, _) in &found {
        assert_eq!(attributes.keys().collect::<Vec<_>>(), vec!["result"]);
    }
    assert!(found.contains(&(labels(&[("result", "success")]), 2)));
    assert!(found.contains(&(labels(&[("result", "provider_error")]), 1)));
    assert!(found.contains(&(labels(&[("result", "base_missing")]), 1)));
}

#[test]
fn the_cas_conflict_and_fallback_counters_have_no_labels() {
    // inst-mt-cas, inst-mt-fallback
    let harness = harness();
    harness.metrics.cas_conflict();
    harness.metrics.cas_conflict();
    harness.metrics.fallback();
    assert_eq!(
        series(&harness, "thread_summary_cas_conflicts"),
        vec![(BTreeMap::new(), 2)]
    );
    assert_eq!(
        series(&harness, "summary_fallback"),
        vec![(BTreeMap::new(), 1)]
    );
}

#[test]
fn the_summary_instruments_are_counters_registered_by_the_foundation() {
    // inst-mt-return: the instruments exist under the prefixed names
    let harness = harness();
    for name in [
        "mini_chat_thread_summary_execution",
        "mini_chat_thread_summary_cas_conflicts",
        "mini_chat_summary_fallback",
        "mini_chat_cleanup_completed",
        "mini_chat_cleanup_retry",
        "mini_chat_cleanup_failed",
        "mini_chat_cleanup_vector_store_with_failed_attachments",
        "mini_chat_secondary_cleanup_skipped",
    ] {
        assert!(
            matches!(
                harness.metrics.instrument(name),
                Some(Instrument::Counter(_))
            ),
            "{name}"
        );
    }
}

// ── the cleanup series ───────────────────────────────────────────────────────

#[test]
fn the_cleanup_counters_carry_the_resource_type() {
    // inst-mt-cleanup: `file` and `vector_store`
    let harness = harness();
    harness.metrics.completed("file");
    harness.metrics.completed("file");
    harness.metrics.completed("vector_store");
    harness.metrics.failed("file");
    harness.metrics.failed("vector_store");
    let completed = series(&harness, "cleanup_completed");
    assert!(
        completed.contains(&(labels(&[("resource_type", "file")]), 2)),
        "{completed:?}"
    );
    assert!(completed.contains(&(labels(&[("resource_type", "vector_store")]), 1)));
    let failed = series(&harness, "cleanup_failed");
    assert!(
        failed.contains(&(labels(&[("resource_type", "file")]), 1)),
        "{failed:?}"
    );
    assert!(failed.contains(&(labels(&[("resource_type", "vector_store")]), 1)));
}

#[test]
fn the_retry_counter_carries_the_resource_type_and_the_reason() {
    // inst-mt-cleanup
    let harness = harness();
    harness.metrics.retry("file", "provider_error");
    harness
        .metrics
        .retry("vector_store", "vector_store_delete_failed");
    harness
        .metrics
        .retry("vector_store", "vector_store_delete_failed");
    let found = series(&harness, "cleanup_retry");
    assert!(
        found.contains(&(
            labels(&[("resource_type", "file"), ("reason", "provider_error")]),
            1
        )),
        "{found:?}"
    );
    assert!(found.contains(&(
        labels(&[
            ("resource_type", "vector_store"),
            ("reason", "vector_store_delete_failed")
        ]),
        2
    )));
}

#[test]
fn the_debt_counter_has_no_labels_and_the_skip_counter_carries_the_provider_kind() {
    // inst-mt-cleanup
    let harness = harness();
    harness.metrics.vector_store_with_failed_attachments();
    harness.metrics.secondary_skipped("anthropic");
    harness.metrics.secondary_skipped("anthropic");
    assert_eq!(
        series(&harness, "cleanup_vector_store_with_failed_attachments"),
        vec![(BTreeMap::new(), 1)]
    );
    assert_eq!(
        series(&harness, "secondary_cleanup_skipped"),
        vec![(labels(&[("provider_kind", "anthropic")]), 2)]
    );
}

#[test]
fn the_chat_cleanup_file_series_is_the_one_the_attachment_cleanup_handler_records() {
    // inst-mt-cleanup: the attachment cleanup handler records the same series for its own path
    let harness = harness();
    harness.metrics.cleanup_completed();
    CleanupMetrics::completed(harness.metrics.as_ref(), "file");
    harness.metrics.cleanup_retry("provider_error");
    CleanupMetrics::retry(harness.metrics.as_ref(), "file", "provider_error");
    harness.metrics.cleanup_failed();
    CleanupMetrics::failed(harness.metrics.as_ref(), "file");
    harness.metrics.secondary_cleanup_skipped("anthropic");
    CleanupMetrics::secondary_skipped(harness.metrics.as_ref(), "anthropic");
    assert_eq!(
        series(&harness, "cleanup_completed"),
        vec![(labels(&[("resource_type", "file")]), 2)]
    );
    assert_eq!(
        series(&harness, "cleanup_retry"),
        vec![(
            labels(&[("resource_type", "file"), ("reason", "provider_error")]),
            2
        )]
    );
    assert_eq!(
        series(&harness, "cleanup_failed"),
        vec![(labels(&[("resource_type", "file")]), 2)]
    );
    assert_eq!(
        series(&harness, "secondary_cleanup_skipped"),
        vec![(labels(&[("provider_kind", "anthropic")]), 2)]
    );
}

// ── the series recorded by the real workers ──────────────────────────────────

#[tokio::test]
async fn the_real_summary_worker_records_success_and_the_declared_series_stay_unrecorded() {
    // inst-mt-execution, inst-mt-deferred, inst-mt-trigger-elsewhere, inst-adm-read-metrics,
    // inst-adm-return: the observed state is the series, no admin route is added
    let rig = maint_rig_with(MaintOptions::new().otlp()).await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 4).await;
    rig.upstream.set_default(Reply::http(
        200,
        &responses_answer(&summary_text("S"), (10, 5)),
    ));
    let payload = summary_payload(
        chat.tenant_id,
        chat.id,
        None,
        (t(40), ids[3]),
        uuid::Uuid::new_v4(),
    );
    let worker = rig.summary_worker(settings_for(SUMMARY_MODEL));
    worker
        .handle(&serde_json::to_vec(&payload).unwrap(), 0)
        .await;
    let harness = rig.otlp.as_ref().unwrap();
    assert_eq!(
        series(harness, "thread_summary_execution"),
        vec![(labels(&[("result", "success")]), 1)]
    );
    assert!(series(harness, "thread_summary_cas_conflicts").is_empty());
    assert!(series(harness, "summary_fallback").is_empty());
    let recorded = harness.recorded();
    for deferred in [
        "cleanup_backlog",
        "summary_regen",
        "outbox_enqueue",
        "outbox_dispatch",
        "outbox_dead",
        "outbox_dead_rows",
        "outbox_pending_age_seconds",
        "outbox_oldest_pending_age_seconds",
        "thread_summary_trigger",
    ] {
        assert!(
            !recorded.contains(deferred),
            "{deferred} must stay unrecorded"
        );
    }
}

#[tokio::test]
async fn the_real_summary_worker_records_the_failure_series() {
    // inst-mt-execution, inst-mt-fallback, inst-mt-cas, inst-adm-summary-alert
    let rig = maint_rig_with(MaintOptions::new().otlp()).await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 4).await;
    let payload = serde_json::to_vec(&summary_payload(
        chat.tenant_id,
        chat.id,
        None,
        (t(40), ids[3]),
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    let worker = rig.summary_worker(settings_for(SUMMARY_MODEL));
    rig.upstream
        .set_default(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    worker.handle(&payload, 0).await;
    rig.upstream.set_default(Reply::http(
        200,
        &responses_answer(&summary_text("S"), (1, 1)),
    ));
    worker.handle(&payload, 1).await;
    worker.handle(&payload, 2).await; // a replay after the commit: the frontier moved
    let harness = rig.otlp.as_ref().unwrap();
    let executions = series(harness, "thread_summary_execution");
    assert!(
        executions.contains(&(labels(&[("result", "provider_error")]), 1)),
        "{executions:?}"
    );
    assert!(
        executions.contains(&(labels(&[("result", "success")]), 1)),
        "{executions:?}"
    );
    assert_eq!(
        series(harness, "summary_fallback"),
        vec![(BTreeMap::new(), 1)]
    );
    assert_eq!(
        series(harness, "thread_summary_cas_conflicts"),
        vec![(BTreeMap::new(), 1)]
    );
}

#[tokio::test]
async fn the_real_chat_cleanup_worker_records_files_the_store_and_the_debt() {
    // inst-mt-cleanup, inst-adm-store-alert, inst-adm-file-debt
    let rig = maint_rig_with(MaintOptions::new().otlp()).await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    crate::test_support::attachments::soft_delete_chat(
        rig.db(),
        chat.id,
        crate::test_support::chats::at(50),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("pending").file_id("file-a"),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    let worker = rig.cleanup_worker(5, true);
    let payload = serde_json::to_vec(&chat_cleanup_payload(
        chat.tenant_id,
        chat.id,
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    worker.handle(&payload, 0).await;
    let harness = rig.otlp.as_ref().unwrap();
    let completed = series(harness, "cleanup_completed");
    assert!(
        completed.contains(&(labels(&[("resource_type", "file")]), 1)),
        "{completed:?}"
    );
    assert!(
        completed.contains(&(labels(&[("resource_type", "vector_store")]), 1)),
        "{completed:?}"
    );
    assert_eq!(
        series(harness, "cleanup_vector_store_with_failed_attachments"),
        vec![(BTreeMap::new(), 1)]
    );
    assert!(
        harness
            .recorded()
            .iter()
            .all(|name| name != "cleanup_backlog")
    );
}

#[tokio::test]
async fn every_label_of_the_workers_is_low_cardinality() {
    // inst-mt-labels: no tenant, user, chat, message, attachment, request, file or vector store
    // identifier, and no free-form error text
    let rig = maint_rig_with(MaintOptions::new().otlp()).await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 4).await;
    rig.upstream.set_default(Reply::http(
        500,
        &json!({ "error": { "message": "SECRET FREE TEXT" } }),
    ));
    let payload = serde_json::to_vec(&summary_payload(
        chat.tenant_id,
        chat.id,
        None,
        (t(40), ids[3]),
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    rig.summary_worker(settings_for(SUMMARY_MODEL))
        .handle(&payload, 0)
        .await;
    crate::test_support::attachments::soft_delete_chat(
        rig.db(),
        chat.id,
        crate::test_support::chats::at(50),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .file_id("file-secret-id")
            .secondary("file_ant_secret", "uploaded"),
    )
    .await;
    seed_store(rig.db(), &chat, Some("vs_secret_id")).await;
    let cleanup = serde_json::to_vec(&chat_cleanup_payload(
        chat.tenant_id,
        chat.id,
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    rig.cleanup_worker(5, false).handle(&cleanup, 0).await;
    let harness = rig.otlp.as_ref().unwrap();
    let allowed = ["result", "resource_type", "reason", "provider_kind"];
    let mut checked = 0;
    for (name, attributes, _) in harness.all_points() {
        if !(name.contains("thread_summary")
            || name.contains("summary_fallback")
            || name.contains("cleanup"))
        {
            continue;
        }
        for (key, value) in &attributes {
            assert!(allowed.contains(&key.as_str()), "{name}: label `{key}`");
            for secret in [
                chat.id.to_string(),
                chat.tenant_id.to_string(),
                "SECRET".to_owned(),
                "file-secret-id".to_owned(),
                "vs_secret_id".to_owned(),
                "file_ant_secret".to_owned(),
            ] {
                assert!(!value.contains(&secret), "{name}: {key}={value}");
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "labels were recorded");
}
