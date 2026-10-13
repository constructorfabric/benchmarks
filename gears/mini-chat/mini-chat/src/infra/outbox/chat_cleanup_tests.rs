#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, UsageEvent, UserLimits};
use parking_lot::Mutex;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::ChatCleanupHandler;
use crate::config::OutboxConfig;
use crate::domain::attachment::{
    ChatCleanup, ChatStorage, FileUpload, IndexingStatus, SecondaryRoute, StorageError,
    StorageFlavor, StorageGateway, StorageRoute,
};
use crate::domain::cleanup::ChatCleanupPayload;
use crate::domain::error::DomainError;
use crate::domain::metrics::NullMetrics;
use crate::domain::ports::{ModelPolicyGateway, UsagePublication};
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::storage::AttachmentRepositoryImpl;
use crate::infra::storage::migrations::all_migrations;

const TS: &str = "'2026-01-01T00:00:00+00:00'";
const MAX_ATTEMPTS: u32 = 5;

/// A storage gateway that records the deletes it is asked for and fails them on demand.
#[derive(Default)]
struct FakeStorage {
    calls: Mutex<Vec<String>>,
    fail_files: AtomicBool,
    fail_stores: AtomicBool,
}

fn unused() -> StorageError {
    StorageError::malformed("not used by the chat cleanup")
}

#[async_trait]
impl StorageGateway for FakeStorage {
    fn chat_storage(&self, _: Uuid, _: &str) -> Result<ChatStorage, DomainError> {
        Err(DomainError::internal("not used by the chat cleanup"))
    }

    fn route_for_backend(&self, _: Uuid, backend: &str) -> Result<StorageRoute, DomainError> {
        Ok(StorageRoute {
            provider_id: backend.to_owned(),
            backend: backend.to_owned(),
            alias: "llm.test".to_owned(),
            flavor: StorageFlavor::Openai,
        })
    }

    async fn upload_file(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
        _: FileUpload,
    ) -> Result<String, StorageError> {
        Err(unused())
    }

    async fn delete_file(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
        file_id: &str,
    ) -> Result<(), StorageError> {
        self.calls.lock().push(format!("file:{file_id}"));
        if self.fail_files.load(Ordering::SeqCst) {
            Err(StorageError::status(500))
        } else {
            Ok(())
        }
    }

    async fn create_vector_store(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
    ) -> Result<String, StorageError> {
        Err(unused())
    }

    async fn add_file_to_store(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
        _: &str,
        _: &str,
        _: Uuid,
    ) -> Result<IndexingStatus, StorageError> {
        Err(unused())
    }

    async fn file_status(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
        _: &str,
        _: &str,
    ) -> Result<IndexingStatus, StorageError> {
        Err(unused())
    }

    async fn delete_vector_store(
        &self,
        _: &SecurityContext,
        _: &StorageRoute,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        self.calls.lock().push(format!("store:{vector_store_id}"));
        if self.fail_stores.load(Ordering::SeqCst) {
            Err(StorageError::status(500))
        } else {
            Ok(())
        }
    }

    async fn upload_secondary(
        &self,
        _: &SecurityContext,
        _: &SecondaryRoute,
        _: FileUpload,
    ) -> Result<String, StorageError> {
        Err(unused())
    }

    async fn delete_secondary(
        &self,
        _: &SecurityContext,
        _: &str,
        _: &str,
    ) -> Result<(), StorageError> {
        Err(unused())
    }
}

/// A policy that is never consulted (no attachment of these tests has a secondary copy).
struct NoPolicy;

#[async_trait]
impl ModelPolicyGateway for NoPolicy {
    async fn current_snapshot(&self, _: Uuid) -> Result<PolicySnapshot, DomainError> {
        Err(DomainError::PolicyUnavailable {
            diagnostic: "no policy in this test".to_owned(),
        })
    }

    async fn snapshot_at(&self, _: Uuid, _: i64) -> Result<PolicySnapshot, DomainError> {
        Err(DomainError::PolicyUnavailable {
            diagnostic: "no policy in this test".to_owned(),
        })
    }

    async fn user_limits(&self, _: Uuid, _: i64) -> Result<UserLimits, DomainError> {
        Err(DomainError::PolicyUnavailable {
            diagnostic: "no policy in this test".to_owned(),
        })
    }

    async fn publish_usage(&self, _: UsageEvent) -> UsagePublication {
        UsagePublication::Reject("no policy in this test".to_owned())
    }
}

struct Harness {
    handler: ChatCleanupHandler,
    storage: Arc<FakeStorage>,
    raw: DatabaseConnection,
    tenant: Uuid,
}

fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

async fn harness() -> Harness {
    let dsn = format!(
        "sqlite:file:mini_chat_cleanup_{}?mode=memory&cache=shared",
        Uuid::new_v4()
    );
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    run_migrations_for_testing(&db, all_migrations())
        .await
        .unwrap();
    let raw = Database::connect(&dsn).await.unwrap();
    let repo = Arc::new(AttachmentRepositoryImpl::new(
        Arc::new(DBProvider::<DomainError>::new(db)),
        Arc::new(OutboxEnqueuer::new(&OutboxConfig::default())),
    ));
    let storage = Arc::new(FakeStorage::default());
    let cleanup = ChatCleanup::new(
        repo,
        storage.clone(),
        Arc::new(NoPolicy),
        Arc::new(NullMetrics),
        MAX_ATTEMPTS,
    );
    Harness {
        handler: ChatCleanupHandler::new(Arc::new(cleanup)),
        storage,
        raw,
        tenant: Uuid::new_v4(),
    }
}

impl Harness {
    async fn exec(&self, sql: String) {
        self.raw.execute_unprepared(&sql).await.unwrap();
    }

    async fn chat(&self, deleted: bool) -> Uuid {
        let id = Uuid::new_v4();
        let deleted_at = if deleted { TS } else { "NULL" };
        self.exec(format!(
            "INSERT INTO chats (id, tenant_id, user_id, model, created_at, updated_at, deleted_at) \
             VALUES ({}, {}, {}, 'm', {TS}, {TS}, {deleted_at})",
            blob(id),
            blob(self.tenant),
            blob(Uuid::new_v4()),
        ))
        .await;
        id
    }

    async fn attachment(&self, chat: Uuid, cleanup: &str) -> Uuid {
        self.attachment_with_file(chat, cleanup, None).await
    }

    async fn attachment_with_file(
        &self,
        chat: Uuid,
        cleanup: &str,
        provider_file: Option<&str>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let file = provider_file.map_or_else(|| "NULL".to_owned(), |file| format!("'{file}'"));
        self.exec(format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, status, \
             attachment_kind, storage_backend, provider_file_id, cleanup_status, created_at, \
             updated_at) VALUES ({}, {}, {}, {}, 'f', 'ready', 'document', 'openai', {file}, \
             '{cleanup}', {TS}, {TS})",
            blob(id),
            blob(self.tenant),
            blob(chat),
            blob(Uuid::new_v4()),
        ))
        .await;
        id
    }

    async fn cleanup_state(&self, attachment: Uuid) -> (String, i64) {
        let row = self
            .raw
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                format!(
                    "SELECT cleanup_status, CAST(cleanup_attempts AS INTEGER) AS attempts \
                     FROM attachments WHERE hex(id) = '{}'",
                    attachment.simple().to_string().to_uppercase()
                ),
            ))
            .await
            .unwrap()
            .unwrap();
        (
            row.try_get("", "cleanup_status").unwrap(),
            row.try_get("", "attempts").unwrap(),
        )
    }

    async fn store_rows(&self, chat: Uuid) -> i64 {
        self.raw
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                format!(
                    "SELECT COUNT(*) AS n FROM chat_vector_stores WHERE hex(chat_id) = '{}'",
                    chat.simple().to_string().to_uppercase()
                ),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }

    async fn vector_store(&self, chat: Uuid) {
        self.exec(format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, created_at) \
             VALUES ({}, {}, {}, 'vs_1', 'openai', {TS})",
            blob(Uuid::new_v4()),
            blob(self.tenant),
            blob(chat),
        ))
        .await;
    }

    fn payload(&self, chat: Uuid) -> ChatCleanupPayload {
        ChatCleanupPayload::for_soft_delete(
            self.tenant,
            chat,
            Uuid::new_v4(),
            OffsetDateTime::now_utc(),
        )
    }

    async fn deliver(&self, payload: &ChatCleanupPayload, attempts: i16) -> MessageResult {
        self.deliver_bytes(serde_json::to_vec(payload).unwrap(), attempts)
            .await
    }

    async fn deliver_bytes(&self, bytes: Vec<u8>, attempts: i16) -> MessageResult {
        let message = OutboxMessage {
            partition_id: 1,
            seq: 1,
            payload: bytes,
            payload_type: "application/json".to_owned(),
            created_at: chrono::Utc::now(),
            attempts,
        };
        self.handler.handle(&message).await
    }
}

#[tokio::test]
async fn a_deleted_chat_with_nothing_to_clean_up_is_acknowledged() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    harness.attachment(chat, "done").await;

    let outcome = harness.deliver(&harness.payload(chat), 0).await;

    assert!(matches!(outcome, MessageResult::Ok), "{outcome:?}");
}

#[tokio::test]
async fn malformed_payloads_and_unknown_reasons_are_rejected() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let mut unknown_reason = serde_json::to_value(harness.payload(chat)).unwrap();
    unknown_reason["reason"] = json!("hard_purge");

    let garbage = harness.deliver_bytes(b"not json".to_vec(), 0).await;
    let reason = harness
        .deliver_bytes(serde_json::to_vec(&unknown_reason).unwrap(), 0)
        .await;

    assert!(matches!(garbage, MessageResult::Reject(_)), "{garbage:?}");
    assert!(matches!(reason, MessageResult::Reject(_)), "{reason:?}");
}

#[tokio::test]
async fn an_active_or_missing_chat_is_rejected() {
    let harness = harness().await;
    let active = harness.chat(false).await;

    let for_active = harness.deliver(&harness.payload(active), 0).await;
    let for_missing = harness.deliver(&harness.payload(Uuid::new_v4()), 0).await;

    assert!(
        matches!(for_active, MessageResult::Reject(_)),
        "{for_active:?}"
    );
    assert!(
        matches!(for_missing, MessageResult::Reject(_)),
        "{for_missing:?}"
    );
}

#[tokio::test]
async fn the_chat_is_looked_up_within_the_payload_tenant_only() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let mut foreign = harness.payload(chat);
    foreign.tenant_id = Uuid::new_v4();

    let outcome = harness.deliver(&foreign, 0).await;

    assert!(matches!(outcome, MessageResult::Reject(_)), "{outcome:?}");
}

#[tokio::test]
async fn a_pending_attachment_is_deleted_at_the_provider_and_then_the_vector_store() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let attachment = harness
        .attachment_with_file(chat, "pending", Some("file-1"))
        .await;
    harness.vector_store(chat).await;

    let outcome = harness.deliver(&harness.payload(chat), 0).await;

    assert!(matches!(outcome, MessageResult::Ok), "{outcome:?}");
    assert_eq!(
        *harness.storage.calls.lock(),
        vec!["file:file-1".to_owned(), "store:vs_1".to_owned()]
    );
    assert_eq!(harness.cleanup_state(attachment).await.0, "done");
    assert_eq!(
        harness.store_rows(chat).await,
        0,
        "the store row is the completion marker"
    );
}

#[tokio::test]
async fn a_failing_file_delete_keeps_the_vector_store_and_asks_for_a_retry() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let attachment = harness
        .attachment_with_file(chat, "pending", Some("file-1"))
        .await;
    harness.vector_store(chat).await;
    harness.storage.fail_files.store(true, Ordering::SeqCst);

    let outcome = harness.deliver(&harness.payload(chat), 0).await;

    assert!(matches!(outcome, MessageResult::Retry), "{outcome:?}");
    assert_eq!(
        harness.cleanup_state(attachment).await,
        ("pending".to_owned(), 1)
    );
    assert_eq!(
        *harness.storage.calls.lock(),
        vec!["file:file-1".to_owned()]
    );
    assert_eq!(harness.store_rows(chat).await, 1);
}

#[tokio::test]
async fn an_attachment_that_exhausts_its_attempts_no_longer_blocks_the_vector_store() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let attachment = harness
        .attachment_with_file(chat, "pending", Some("file-1"))
        .await;
    harness
        .exec(format!(
            "UPDATE attachments SET cleanup_attempts = {} WHERE hex(id) = '{}'",
            MAX_ATTEMPTS - 1,
            attachment.simple().to_string().to_uppercase()
        ))
        .await;
    harness.vector_store(chat).await;
    harness.storage.fail_files.store(true, Ordering::SeqCst);

    let outcome = harness.deliver(&harness.payload(chat), 2).await;

    assert!(matches!(outcome, MessageResult::Ok), "{outcome:?}");
    assert_eq!(
        harness.cleanup_state(attachment).await,
        ("failed".to_owned(), i64::from(MAX_ATTEMPTS))
    );
    assert_eq!(harness.store_rows(chat).await, 0);
}

#[tokio::test]
async fn a_failing_vector_store_delete_defers_the_message_until_the_attempt_limit() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    harness.attachment(chat, "done").await;
    harness.vector_store(chat).await;
    harness.storage.fail_stores.store(true, Ordering::SeqCst);

    let first = harness.deliver(&harness.payload(chat), 0).await;
    let last = harness
        .deliver(
            &harness.payload(chat),
            i16::try_from(MAX_ATTEMPTS - 1).unwrap(),
        )
        .await;

    assert!(matches!(first, MessageResult::Retry), "{first:?}");
    assert!(
        matches!(&last, MessageResult::Reject(reason) if reason.contains("max attempts")),
        "{last:?}"
    );
    assert_eq!(
        harness.store_rows(chat).await,
        1,
        "the row is kept for a replay"
    );
}

#[tokio::test]
async fn attachments_without_a_provider_file_are_marked_done_without_a_call() {
    let harness = harness().await;
    let chat = harness.chat(true).await;
    let attachment = harness.attachment(chat, "pending").await;

    let outcome = harness.deliver(&harness.payload(chat), 0).await;

    assert!(matches!(outcome, MessageResult::Ok), "{outcome:?}");
    assert!(harness.storage.calls.lock().is_empty());
    assert_eq!(harness.cleanup_state(attachment).await.0, "done");
}
