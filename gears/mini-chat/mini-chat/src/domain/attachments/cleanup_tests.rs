//! The attachment cleanup handler logic over an in-memory store, a delete-only provider and the
//! recording metrics: the payload contract, the soft-deleted and missing parent chat, the missing
//! and `done` rows, the null provider file id, the provider delete (2xx and 404 are success), the
//! retry budget with `Retry` and the `failed` plus `Reject` at the limit, the unresolvable
//! `storage_backend`, database errors without an attempt increment, the best-effort secondary
//! delete, the absence of any Vector Stores call, the dead-letter replay of a `failed` row (one
//! delete attempt, `done` or `Reject` without attempt growth) and the cleanup status state
//! machine.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` (`inst-ch-*`),
//! state `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status` (`inst-cs-*`),
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability` (`inst-adm-replay`),
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics` (`inst-met-cleanup`);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    AttachmentCleanupWorker, CleanupDeps, CleanupFrom, CleanupOutcome, CleanupPayload,
    parse_payload, secondary_target, storage_target_for_backend,
};
use crate::config::StorageKind;
use crate::domain::attachments::provider::{ProviderFailure, StorageProvider};
use crate::domain::attachments::store::SecondaryRef;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;
use crate::test_support::attach_fakes::RecordingAttachmentMetrics;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, AZURE_API_VERSION, AZURE_BACKEND, AZURE_RAG_ALIAS, OPENAI_ALIAS,
    OPENAI_BACKEND, providers,
};
use crate::test_support::cleanup_fakes::{CleanupCall, DeletingProvider, FakeCleanupStore};
use crate::test_support::manual_time::ManualTime;
use crate::test_support::quota::default_now;
use crate::test_support::{TENANT_A, TENANT_B};

const CHAT: Uuid = Uuid::from_u128(0xC1);
const ATTACHMENT: Uuid = Uuid::from_u128(0xA7);
const FILE: &str = "file-abc";
const SECONDARY_FILE: &str = "file_ant_9";

// ── the harness ──────────────────────────────────────────────────────────────

struct Harness {
    worker: AttachmentCleanupWorker,
    store: Arc<FakeCleanupStore>,
    provider: Arc<DeletingProvider>,
    secondary: Arc<DeletingProvider>,
    metrics: Arc<RecordingAttachmentMetrics>,
}

fn build(
    store: Arc<FakeCleanupStore>,
    with_secondary_client: bool,
    max_attempts: u32,
    registry: Arc<ProviderRegistry>,
) -> Harness {
    let provider = DeletingProvider::new();
    let secondary = DeletingProvider::new();
    let metrics = RecordingAttachmentMetrics::new();
    let clock: Arc<dyn Clock> = ManualTime::auto(default_now());
    let worker = AttachmentCleanupWorker::new(CleanupDeps {
        store: store.clone(),
        storage: provider.clone(),
        secondary_storage: with_secondary_client
            .then(|| Arc::clone(&secondary) as Arc<dyn StorageProvider>),
        providers: registry,
        clock,
        metrics: metrics.clone(),
        max_attempts,
    });
    Harness {
        worker,
        store,
        provider,
        secondary,
        metrics,
    }
}

fn harness(cleanup_status: Option<&str>, attempts: i32) -> Harness {
    build(
        FakeCleanupStore::with_row(cleanup_status, attempts),
        true,
        5,
        providers(),
    )
}

fn pending() -> Harness {
    harness(Some("pending"), 0)
}

/// A payload as the queue serializes it.
fn payload_value(event: &str, file_id: Option<&str>, backend: &str) -> Value {
    json!({
        "event_type": event,
        "tenant_id": TENANT_A,
        "chat_id": CHAT,
        "attachment_id": ATTACHMENT,
        "provider_file_id": file_id,
        "vector_store_id": Value::Null,
        "storage_backend": backend,
        "attachment_kind": "document",
        "deleted_at": "2026-02-28T15:30:00Z",
        "secondary_ref": Value::Null,
    })
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

/// The `attachment_deleted` payload of a file on the default storage.
fn deleted_payload() -> Vec<u8> {
    bytes(&payload_value(
        "attachment_deleted",
        Some(FILE),
        OPENAI_BACKEND,
    ))
}

fn with_secondary(mut value: Value) -> Value {
    value["secondary_ref"] = json!({
        "file_id": SECONDARY_FILE,
        "provider_kind": "anthropic",
        "upstream_alias": ANTHROPIC_ALIAS,
    });
    value
}

fn assert_reject(outcome: &CleanupOutcome) {
    assert!(matches!(outcome, CleanupOutcome::Reject(_)), "{outcome:?}");
}

// ── the payload contract ─────────────────────────────────────────────────────

#[test]
fn a_well_formed_payload_is_parsed_field_by_field() {
    // inst-ch-parse
    let payload = parse_payload(&bytes(&with_secondary(payload_value(
        "attachment_deleted",
        Some(FILE),
        AZURE_BACKEND,
    ))))
    .expect("a valid payload");
    assert_eq!(
        payload,
        CleanupPayload {
            event_type: "attachment_deleted".to_owned(),
            tenant_id: TENANT_A,
            chat_id: CHAT,
            attachment_id: ATTACHMENT,
            provider_file_id: Some(FILE.to_owned()),
            storage_backend: AZURE_BACKEND.to_owned(),
            secondary_ref: Some(SecondaryRef {
                file_id: SECONDARY_FILE.to_owned(),
                provider_kind: "anthropic".to_owned(),
                upstream_alias: ANTHROPIC_ALIAS.to_owned(),
            }),
        }
    );
}

#[test]
fn a_null_provider_file_id_and_a_null_secondary_ref_are_valid() {
    // inst-ch-parse: the upload never reached the provider
    let payload = parse_payload(&bytes(&payload_value(
        "attachment_upload_abandoned",
        None,
        OPENAI_BACKEND,
    )))
    .expect("a valid payload");
    assert_eq!(payload.provider_file_id, None);
    assert_eq!(payload.secondary_ref, None);
}

#[test]
fn every_event_type_of_the_queue_is_accepted() {
    for event in [
        "attachment_deleted",
        "attachment_upload_abandoned",
        "attachment_indexing_failed",
    ] {
        let payload = parse_payload(&bytes(&payload_value(event, Some(FILE), OPENAI_BACKEND)))
            .unwrap_or_else(|reason| panic!("{event}: {reason}"));
        assert_eq!(payload.event_type, event);
    }
}

#[test]
fn a_malformed_payload_is_refused_with_a_reason() {
    // inst-ch-malformed
    let mut cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"not json".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"{}".to_vec(),
    ];
    for field in ["tenant_id", "chat_id", "attachment_id", "storage_backend"] {
        let mut value = payload_value("attachment_deleted", Some(FILE), OPENAI_BACKEND);
        value.as_object_mut().unwrap().remove(field);
        cases.push(bytes(&value));
    }
    for (field, wrong) in [
        ("tenant_id", json!("not-a-uuid")),
        ("chat_id", json!(7)),
        ("attachment_id", json!(null)),
        ("provider_file_id", json!(12)),
        ("event_type", json!("attachment_exploded")),
        ("secondary_ref", json!("a string")),
    ] {
        let mut value = payload_value("attachment_deleted", Some(FILE), OPENAI_BACKEND);
        value[field] = wrong;
        cases.push(bytes(&value));
    }
    for case in cases {
        let reason = parse_payload(&case).expect_err(&String::from_utf8_lossy(&case));
        assert!(!reason.is_empty());
    }
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_at_once_without_any_read_or_call() {
    // inst-ch-malformed, inst-ch-return-malformed
    for case in [
        b"not json".to_vec(),
        Vec::new(),
        bytes(&json!({ "event_type": "attachment_deleted" })),
    ] {
        let h = pending();
        let outcome = h.worker.handle(&case).await;
        assert_reject(&outcome);
        assert!(
            h.store.calls().is_empty(),
            "no query for a malformed payload"
        );
        assert!(h.provider.calls().is_empty());
        assert!(h.secondary.calls().is_empty());
        assert_eq!(h.store.row().unwrap().cleanup_attempts, 0);
    }
}

// ── the storage dispatcher ───────────────────────────────────────────────────

#[test]
fn the_storage_backend_label_maps_to_its_storage_provider() {
    // inst-ch-delete-file: the storage dispatcher maps `storage_backend`
    let registry = providers();
    let openai = storage_target_for_backend(&registry, TENANT_A, OPENAI_BACKEND).expect("openai");
    assert_eq!(openai.upstream_alias, OPENAI_ALIAS);
    assert_eq!(openai.storage_kind, StorageKind::Openai);
    assert_eq!(openai.storage_backend, OPENAI_BACKEND);
    let azure = storage_target_for_backend(&registry, TENANT_A, AZURE_BACKEND).expect("azure");
    assert_eq!(azure.upstream_alias, AZURE_RAG_ALIAS);
    assert_eq!(azure.storage_kind, StorageKind::Azure);
    assert_eq!(azure.api_version.as_deref(), Some(AZURE_API_VERSION));
    assert_eq!(azure.storage_backend, AZURE_BACKEND);
}

#[test]
fn an_unknown_storage_backend_has_no_target() {
    // inst-ch-delete-file: an unresolvable `storage_backend` counts as a failed attempt
    let registry = providers();
    for label in ["", "nope", "AZURE_OPENAI", "azure_openai "] {
        assert_eq!(
            storage_target_for_backend(&registry, TENANT_A, label),
            None,
            "{label:?}"
        );
    }
}

#[test]
fn the_tenant_override_alias_applies_to_the_storage_target() {
    let config = crate::config::MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": {
                "kind": "openai_responses", "host": OPENAI_ALIAS, "storage_kind": "openai",
                "tenant_overrides": {
                    TENANT_B.hyphenated().to_string(): { "host": "tenant-b.example.com" }
                }
            }
        }
    }))
    .expect("valid config");
    let registry = ProviderRegistry::build(&config.providers);
    let a = storage_target_for_backend(&registry, TENANT_A, "azure_openai").unwrap();
    let b = storage_target_for_backend(&registry, TENANT_B, "azure_openai").unwrap();
    assert_eq!(a.upstream_alias, OPENAI_ALIAS);
    assert_eq!(b.upstream_alias, "tenant-b.example.com");
}

#[test]
fn the_secondary_target_is_the_anthropic_upstream_of_the_secondary_ref() {
    // inst-ch-delete-secondary: the Anthropic Files client on `secondary_ref.upstream_alias`
    let target = secondary_target(&SecondaryRef {
        file_id: SECONDARY_FILE.to_owned(),
        provider_kind: "anthropic".to_owned(),
        upstream_alias: ANTHROPIC_ALIAS.to_owned(),
    });
    assert_eq!(target.upstream_alias, ANTHROPIC_ALIAS);
    assert_eq!(
        target.storage_kind,
        StorageKind::Openai,
        "the /v1 file prefix"
    );
}

// ── the parent chat ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_soft_deleted_parent_chat_is_acknowledged_without_action() {
    // inst-ch-load-chat, inst-ch-chat-deleted, inst-ch-return-chat-owned
    let store = FakeCleanupStore::with_row(Some("pending"), 0).chat_deleted();
    let h = build(store, true, 5, providers());
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert!(h.provider.calls().is_empty(), "chat cleanup owns the files");
    assert!(h.store.writes().is_empty());
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(h.metrics.cleanup_completed(), 0);
}

#[tokio::test]
async fn a_missing_parent_chat_is_acknowledged_like_a_soft_deleted_one() {
    // the feature-local deviation: a missing chat is acked, not retried
    let store = FakeCleanupStore::with_row(Some("pending"), 0).chat_missing();
    let h = build(store, true, 5, providers());
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert!(h.provider.calls().is_empty());
    assert!(h.store.writes().is_empty());
}

#[tokio::test]
async fn a_deleted_parent_chat_is_acknowledged_for_a_failed_row_too() {
    // the replay branch runs only when the parent chat is not soft-deleted (step 4 precedes it)
    let store = FakeCleanupStore::with_row(Some("failed"), 5).chat_deleted();
    let h = build(store, true, 5, providers());
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert!(h.provider.calls().is_empty());
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
}

// ── the row ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_row_is_acknowledged_without_action() {
    // inst-ch-load-row, inst-ch-terminal, inst-ch-return-terminal
    let store = FakeCleanupStore::with_row(Some("pending"), 0).row_missing();
    let h = build(store, true, 5, providers());
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert!(h.provider.calls().is_empty());
    assert!(h.store.writes().is_empty());
}

#[tokio::test]
async fn a_done_row_is_acknowledged_without_a_provider_call() {
    // inst-ch-terminal: a redelivery or a replay of a finished message
    let h = harness(Some("done"), 2);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert!(h.provider.calls().is_empty());
    assert!(h.store.writes().is_empty());
    assert_eq!(
        h.metrics.cleanup_completed(),
        0,
        "nothing was completed now"
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 2);
}

#[tokio::test]
async fn the_queries_are_the_chat_read_then_the_row_read() {
    // inst-ch-load-chat before inst-ch-load-row
    let h = pending();
    h.worker.handle(&deleted_payload()).await;
    let reads: Vec<_> = h
        .store
        .calls()
        .into_iter()
        .filter(|call| matches!(call, CleanupCall::ChatRead | CleanupCall::RowRead))
        .collect();
    assert_eq!(reads, vec![CleanupCall::ChatRead, CleanupCall::RowRead]);
}

#[tokio::test]
async fn a_chat_read_error_retries_without_incrementing_attempts() {
    // inst-ch-db-error, inst-ch-return-db-retry
    let h = harness(Some("pending"), 3);
    h.store
        .fail_chat_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert!(h.provider.calls().is_empty());
    assert!(h.store.writes().is_empty());
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 3);
    assert_eq!(
        h.metrics.cleanup_retries(),
        Vec::<String>::new(),
        "no provider error"
    );
}

#[tokio::test]
async fn a_row_read_error_retries_without_incrementing_attempts() {
    // inst-ch-db-error: a database error in steps 3 to 5
    let h = harness(Some("pending"), 3);
    h.store
        .fail_row_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert!(h.provider.calls().is_empty());
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 3);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("pending")
    );
}

// ── no provider file ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_null_provider_file_id_marks_the_cleanup_done_without_a_provider_call() {
    // inst-ch-no-file, inst-ch-return-no-file, inst-ch-mark-done
    for event in [
        "attachment_deleted",
        "attachment_upload_abandoned",
        "attachment_indexing_failed",
    ] {
        let h = pending();
        let outcome = h
            .worker
            .handle(&bytes(&payload_value(event, None, OPENAI_BACKEND)))
            .await;
        assert_eq!(outcome, CleanupOutcome::Ok, "{event}");
        assert!(h.provider.calls().is_empty(), "{event}");
        let row = h.store.row().unwrap();
        assert_eq!(row.cleanup_status.as_deref(), Some("done"), "{event}");
        assert_eq!(row.cleanup_attempts, 0);
        assert_eq!(h.metrics.cleanup_completed(), 1, "{event}");
        assert_eq!(
            h.store.writes(),
            vec![CleanupCall::MarkDone(CleanupFrom::Pending)]
        );
    }
}

// ── the provider delete ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_2xx_marks_the_cleanup_done() {
    // inst-ch-delete-file, inst-ch-mark-done, inst-ch-return-ok, inst-cs-done
    let h = pending();
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    let calls = h.provider.primary_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].file_id, FILE);
    assert_eq!(calls[0].upstream_alias, OPENAI_ALIAS);
    let row = h.store.row().unwrap();
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0, "a success records no attempt");
    assert_eq!(row.cleanup_updated_at, Some(default_now()));
    assert_eq!(h.metrics.cleanup_completed(), 1);
    assert!(h.metrics.cleanup_retries().is_empty());
    assert_eq!(h.metrics.cleanup_failed(), 0);
}

#[tokio::test]
async fn the_delete_goes_to_the_provider_the_storage_backend_maps_to() {
    // inst-ch-delete-file: the storage dispatcher maps `storage_backend`
    let h = pending();
    let outcome = h
        .worker
        .handle(&bytes(&payload_value(
            "attachment_deleted",
            Some(FILE),
            AZURE_BACKEND,
        )))
        .await;
    assert_eq!(outcome, CleanupOutcome::Ok);
    let calls = h.provider.primary_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].upstream_alias, AZURE_RAG_ALIAS);
    assert_eq!(calls[0].storage_backend, AZURE_BACKEND);
}

#[tokio::test]
async fn a_provider_404_counts_as_success() {
    // inst-ch-delete-failed (negative): 2xx or 404
    let h = pending();
    h.provider.queue_primary(Err(ProviderFailure::NotFound));
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 0);
    assert_eq!(h.metrics.cleanup_completed(), 1);
}

#[tokio::test]
async fn the_handler_makes_no_vector_store_call() {
    // inst-ch-no-vector-store: the document leaves the store when its provider file is deleted
    for failure in [
        None,
        Some(ProviderFailure::Other),
        Some(ProviderFailure::NotFound),
    ] {
        let h = pending();
        if let Some(failure) = failure {
            h.provider.queue_primary(Err(failure));
        }
        h.worker
            .handle(&bytes(&{
                let mut value = payload_value("attachment_deleted", Some(FILE), OPENAI_BACKEND);
                value["vector_store_id"] = json!("vs_must_be_ignored");
                value
            }))
            .await;
        assert_eq!(h.provider.other_calls(), 0, "{failure:?}");
        assert_eq!(h.secondary.other_calls(), 0, "{failure:?}");
    }
}

#[tokio::test]
async fn a_failed_provider_delete_records_the_attempt_and_retries() {
    // inst-ch-delete-failed, inst-ch-record-attempt, inst-ch-return-retry, inst-cs-retry
    for failure in [ProviderFailure::Other, ProviderFailure::Transient] {
        let h = pending();
        h.provider.queue_primary(Err(failure));
        assert_eq!(
            h.worker.handle(&deleted_payload()).await,
            CleanupOutcome::Retry,
            "{failure:?}"
        );
        let row = h.store.row().unwrap();
        assert_eq!(
            row.cleanup_status.as_deref(),
            Some("pending"),
            "{failure:?}"
        );
        assert_eq!(row.cleanup_attempts, 1, "{failure:?}");
        assert!(row.last_cleanup_error.is_some(), "the cause is recorded");
        assert_eq!(row.cleanup_updated_at, Some(default_now()));
        assert_eq!(
            h.metrics.cleanup_retries(),
            vec!["provider_error".to_owned()]
        );
        assert_eq!(h.metrics.cleanup_failed(), 0);
        assert_eq!(h.metrics.cleanup_completed(), 0);
    }
}

#[tokio::test]
async fn the_recorded_cause_holds_no_provider_text_or_file_identifier() {
    // admin-observability: `last_cleanup_error` holds no file content
    let h = pending();
    h.provider.queue_primary(Err(ProviderFailure::Other));
    h.worker.handle(&deleted_payload()).await;
    let cause = h.store.row().unwrap().last_cleanup_error.unwrap();
    assert!(!cause.contains(FILE), "{cause}");
    assert!(!cause.contains("secret"), "{cause}");
}

#[tokio::test]
async fn the_fifth_failed_attempt_marks_the_cleanup_failed_and_rejects() {
    // inst-ch-exhausted, inst-cs-failed, inst-cs-retry: the default budget is 5
    let h = pending();
    for attempt in 1..=4 {
        h.provider.queue_primary(Err(ProviderFailure::Other));
        assert_eq!(
            h.worker.handle(&deleted_payload()).await,
            CleanupOutcome::Retry,
            "attempt {attempt}"
        );
        let row = h.store.row().unwrap();
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        assert_eq!(row.cleanup_attempts, attempt);
    }
    h.provider.queue_primary(Err(ProviderFailure::Transient));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    let row = h.store.row().unwrap();
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 5);
    assert_eq!(h.metrics.cleanup_failed(), 1);
    assert_eq!(
        h.metrics.cleanup_retries().len(),
        4,
        "the exhausting delivery is no retry"
    );
    assert_eq!(h.provider.primary_calls().len(), 5);
}

#[tokio::test]
async fn the_budget_is_cleanup_worker_max_attempts() {
    // inst-ch-exhausted: `cleanup_worker.max_attempts`
    let store = FakeCleanupStore::with_row(Some("pending"), 0);
    let h = build(store, true, 2, providers());
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 2);
}

#[tokio::test]
async fn a_row_that_was_already_at_the_last_attempt_fails_on_its_next_failure() {
    // a row seeded with four attempts fails on the fifth failed delete
    let h = harness(Some("pending"), 4);
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(h.metrics.cleanup_failed(), 1);
}

#[tokio::test]
async fn an_unresolvable_storage_backend_is_a_failed_attempt_without_a_provider_call() {
    // inst-ch-delete-file: bounded by the retry budget
    let h = pending();
    let payload = bytes(&payload_value(
        "attachment_deleted",
        Some(FILE),
        "retired-backend",
    ));
    for attempt in 1..=4 {
        assert_eq!(
            h.worker.handle(&payload).await,
            CleanupOutcome::Retry,
            "{attempt}"
        );
        assert_eq!(h.store.row().unwrap().cleanup_attempts, attempt);
    }
    assert_reject(&h.worker.handle(&payload).await);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    assert!(h.provider.calls().is_empty(), "no provider is mapped");
    assert_eq!(h.metrics.cleanup_failed(), 1);
}

#[tokio::test]
async fn an_attempt_write_error_retries_without_changing_the_row() {
    // inst-ch-record-attempt on a database failure: an infrastructure error never grows attempts
    let h = pending();
    h.provider.queue_primary(Err(ProviderFailure::Other));
    h.store
        .fail_attempt
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 0);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn a_done_write_error_retries_and_the_redelivery_repeats_the_idempotent_delete() {
    // a database error at the `done` write: `Retry`, the row stays `pending`, the redelivery
    // deletes again (404 or 2xx) and completes
    let h = pending();
    h.store
        .fail_done
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 0);
    h.store
        .fail_done
        .store(false, std::sync::atomic::Ordering::SeqCst);
    h.provider.queue_primary(Err(ProviderFailure::NotFound));
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(h.provider.primary_calls().len(), 2);
}

#[tokio::test]
async fn every_event_type_runs_the_same_delete() {
    // the abandoned upload and the failed indexing rows are `failed` but not soft-deleted; the
    // message is handled identically
    for event in [
        "attachment_deleted",
        "attachment_upload_abandoned",
        "attachment_indexing_failed",
    ] {
        let h = pending();
        let outcome = h
            .worker
            .handle(&bytes(&payload_value(event, Some(FILE), OPENAI_BACKEND)))
            .await;
        assert_eq!(outcome, CleanupOutcome::Ok, "{event}");
        assert_eq!(h.provider.primary_calls().len(), 1, "{event}");
        assert_eq!(
            h.store.row().unwrap().cleanup_status.as_deref(),
            Some("done")
        );
    }
}

// ── the secondary copy ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_secondary_file_is_deleted_on_the_anthropic_upstream_after_the_primary() {
    // inst-ch-secondary, inst-ch-delete-secondary
    let h = pending();
    let payload = bytes(&with_secondary(payload_value(
        "attachment_deleted",
        Some(FILE),
        OPENAI_BACKEND,
    )));
    assert_eq!(h.worker.handle(&payload).await, CleanupOutcome::Ok);
    assert_eq!(h.provider.primary_calls().len(), 1);
    let secondary = h.secondary.secondary_calls();
    assert_eq!(secondary.len(), 1, "{secondary:?}");
    assert_eq!(secondary[0].file_id, SECONDARY_FILE);
    assert_eq!(secondary[0].upstream_alias, ANTHROPIC_ALIAS);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
    assert!(h.metrics.secondary_skipped().is_empty());
}

#[tokio::test]
async fn a_failed_secondary_delete_does_not_block_the_done_transition() {
    // inst-ch-delete-secondary: a failure is logged and does not block the next step
    for failure in [ProviderFailure::Other, ProviderFailure::Transient] {
        let h = pending();
        h.secondary.queue_secondary(Err(failure));
        let payload = bytes(&with_secondary(payload_value(
            "attachment_deleted",
            Some(FILE),
            OPENAI_BACKEND,
        )));
        assert_eq!(
            h.worker.handle(&payload).await,
            CleanupOutcome::Ok,
            "{failure:?}"
        );
        assert_eq!(
            h.store.row().unwrap().cleanup_status.as_deref(),
            Some("done")
        );
        assert_eq!(h.store.row().unwrap().cleanup_attempts, 0);
        assert_eq!(h.metrics.cleanup_completed(), 1);
        assert!(h.metrics.cleanup_retries().is_empty());
    }
}

#[tokio::test]
async fn the_secondary_copy_is_left_alone_while_the_primary_delete_fails() {
    // the secondary delete is step 12, after the primary delete succeeded
    let h = pending();
    h.provider.queue_primary(Err(ProviderFailure::Other));
    let payload = bytes(&with_secondary(payload_value(
        "attachment_deleted",
        Some(FILE),
        OPENAI_BACKEND,
    )));
    assert_eq!(h.worker.handle(&payload).await, CleanupOutcome::Retry);
    assert!(h.secondary.secondary_calls().is_empty());
}

#[tokio::test]
async fn without_an_anthropic_files_client_the_skip_is_counted_and_the_cleanup_completes() {
    // inst-ch-delete-secondary: `mini_chat_secondary_cleanup_skipped{provider_kind}`
    let store = FakeCleanupStore::with_row(Some("pending"), 0);
    let h = build(store, false, 5, providers());
    let payload = bytes(&with_secondary(payload_value(
        "attachment_deleted",
        Some(FILE),
        OPENAI_BACKEND,
    )));
    assert_eq!(h.worker.handle(&payload).await, CleanupOutcome::Ok);
    assert_eq!(h.metrics.secondary_skipped(), vec!["anthropic".to_owned()]);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
    assert!(h.secondary.calls().is_empty());
}

#[tokio::test]
async fn without_a_secondary_ref_no_secondary_delete_and_no_skip_metric() {
    let h = pending();
    h.worker.handle(&deleted_payload()).await;
    assert!(h.secondary.calls().is_empty());
    assert!(h.metrics.secondary_skipped().is_empty());
}

// ── the dead-letter replay of a `failed` row ─────────────────────────────────

#[tokio::test]
async fn a_replay_with_a_provider_success_sets_the_row_done() {
    // inst-ch-replay, inst-ch-replay-delete, inst-ch-replay-ok, inst-ch-replay-done,
    // inst-cs-replay-done, inst-adm-replay
    let h = harness(Some("failed"), 5);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(h.provider.primary_calls().len(), 1, "exactly one attempt");
    let row = h.store.row().unwrap();
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 5, "the attempts stay at the budget");
    assert_eq!(h.metrics.cleanup_completed(), 1);
    assert_eq!(
        h.store.writes(),
        vec![CleanupCall::MarkDone(CleanupFrom::Failed)],
        "the guarded `failed` to `done` transition"
    );
}

#[tokio::test]
async fn a_replay_with_a_provider_404_sets_the_row_done() {
    // inst-ch-replay-ok: 2xx or 404
    let h = harness(Some("failed"), 5);
    h.provider.queue_primary(Err(ProviderFailure::NotFound));
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn a_replay_deletes_the_secondary_copy_best_effort_before_done() {
    // inst-ch-replay-secondary
    let h = harness(Some("failed"), 5);
    h.secondary.queue_secondary(Err(ProviderFailure::Other));
    let payload = bytes(&with_secondary(payload_value(
        "attachment_deleted",
        Some(FILE),
        OPENAI_BACKEND,
    )));
    assert_eq!(h.worker.handle(&payload).await, CleanupOutcome::Ok);
    assert_eq!(h.secondary.secondary_calls().len(), 1);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn a_replay_with_a_provider_failure_keeps_the_row_failed_and_rejects_again() {
    // inst-ch-replay-failed, inst-cs-replay-failed: no loop, no attempt growth, no new metric
    for failure in [ProviderFailure::Other, ProviderFailure::Transient] {
        let h = harness(Some("failed"), 5);
        h.provider.queue_primary(Err(failure));
        assert_reject(&h.worker.handle(&deleted_payload()).await);
        let row = h.store.row().unwrap();
        assert_eq!(row.cleanup_status.as_deref(), Some("failed"), "{failure:?}");
        assert_eq!(row.cleanup_attempts, 5, "{failure:?}");
        assert!(row.last_cleanup_error.is_some(), "the cause is refreshed");
        assert_eq!(row.cleanup_updated_at, Some(default_now()));
        assert_eq!(h.provider.primary_calls().len(), 1);
        assert_eq!(h.metrics.cleanup_failed(), 0, "no second `cleanup_failed`");
        assert!(h.metrics.cleanup_retries().is_empty(), "no `cleanup_retry`");
        assert_eq!(h.metrics.cleanup_completed(), 0);
    }
}

#[tokio::test]
async fn a_replay_never_returns_retry_for_a_provider_failure() {
    // the handler never returns `Retry` for a replay: no automatic loop
    let h = harness(Some("failed"), 5);
    for _ in 0..3 {
        h.provider.queue_primary(Err(ProviderFailure::Transient));
        assert_reject(&h.worker.handle(&deleted_payload()).await);
    }
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 5);
    assert_eq!(
        h.provider.primary_calls().len(),
        3,
        "one attempt per operator replay"
    );
}

#[tokio::test]
async fn a_replay_of_an_unresolvable_backend_counts_as_a_failed_attempt_and_rejects() {
    // inst-ch-replay-delete: an unresolvable `storage_backend` counts as a failed attempt
    let h = harness(Some("failed"), 5);
    let payload = bytes(&payload_value(
        "attachment_deleted",
        Some(FILE),
        "retired-backend",
    ));
    assert_reject(&h.worker.handle(&payload).await);
    let row = h.store.row().unwrap();
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 5);
    assert!(h.provider.calls().is_empty());
}

#[tokio::test]
async fn a_replay_done_write_error_retries_without_changing_attempts() {
    // inst-ch-replay-done: on a database error `Retry`; the redelivery repeats the delete
    let h = harness(Some("failed"), 5);
    h.store
        .fail_done
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 5);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    h.store
        .fail_done
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        h.provider.primary_calls().len(),
        2,
        "the delete is idempotent"
    );
}

#[tokio::test]
async fn a_replay_failure_write_error_retries_without_changing_attempts() {
    // a database error while recording the replay failure is an infrastructure error
    let h = harness(Some("failed"), 5);
    h.provider.queue_primary(Err(ProviderFailure::Other));
    h.store
        .fail_replay_failure
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Retry
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 5);
}

#[tokio::test]
async fn a_second_replay_after_the_row_is_done_is_acknowledged() {
    // a replay of a `done` row is acknowledged without a provider call
    let h = harness(Some("failed"), 5);
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(h.provider.primary_calls().len(), 1);
    assert_eq!(h.metrics.cleanup_completed(), 1);
}

#[tokio::test]
async fn a_redelivery_after_a_crash_before_the_reject_takes_the_replay_branch() {
    // inst-ch-replay: the exhausting delivery set `failed` and crashed before its `Reject`; the
    // redelivery sees a `failed` row, makes one attempt and never grows the attempts
    let h = pending();
    for _ in 0..4 {
        h.provider.queue_primary(Err(ProviderFailure::Other));
        h.worker.handle(&deleted_payload()).await;
    }
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    // the redelivery of the same message
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 5);
    assert_eq!(h.metrics.cleanup_failed(), 1);
}

// ── the cleanup status state machine ─────────────────────────────────────────

#[tokio::test]
async fn the_state_machine_walks_pending_to_failed_to_done() {
    // inst-cs-retry, inst-cs-failed, inst-cs-replay-failed, inst-cs-replay-done
    let h = pending();
    // pending -> pending (below the budget)
    h.provider.queue_primary(Err(ProviderFailure::Other));
    h.worker.handle(&deleted_payload()).await;
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("pending")
    );
    // pending -> failed (at the budget)
    for _ in 0..4 {
        h.provider.queue_primary(Err(ProviderFailure::Other));
        h.worker.handle(&deleted_payload()).await;
    }
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    // failed -> failed (a replay that fails)
    h.provider.queue_primary(Err(ProviderFailure::Other));
    assert_reject(&h.worker.handle(&deleted_payload()).await);
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(h.store.row().unwrap().cleanup_attempts, 5);
    // failed -> done (a replay that succeeds): the only transition out of `failed`
    assert_eq!(
        h.worker.handle(&deleted_payload()).await,
        CleanupOutcome::Ok
    );
    assert_eq!(
        h.store.row().unwrap().cleanup_status.as_deref(),
        Some("done")
    );
}
