#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Chat cleanup handler tests on in-memory `SQLite` with a scripted fake
//! provider storage (DESIGN section 3.6 "Cleanup on Chat Deletion",
//! "Attachment cleanup state machine", "Vector store cleanup ordering").

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::secure::SecureUpdateExt;

use super::*;
use crate::config::ProviderEntry;
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, FileUpload, NewVectorStorePlaceholder, StorageError,
    VectorStoreFileStatus,
};
use crate::infra::db::entities::attachments;
use crate::infra::db::repo::chats as chat_repo;
use crate::infra::db::repo::tests::{
    AttachmentSeed, TENANT_A, TestStore, USER_A1, at, attachment_row, seed_attachment, seed_chat,
};
use crate::infra::llm::fake_gateway::{FakeGateway, StaticSystemContext, json_response, lock};
use crate::infra::llm::storage::ProviderStorage;
use crate::infra::outbox::ChatCleanupReason;

const MAX_ATTEMPTS: u32 = 2;

/// One provider call of the fake storage, in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    File(String),
    Store(String),
    Secondary(String, String),
}

/// Longer than any test call timeout: a delete that never answers in time.
const HANG: Duration = Duration::from_secs(60);

/// Short call timeout of the hanging-provider tests.
const CALL_TIMEOUT: Duration = Duration::from_millis(50);

/// Provider storage whose deletes fail while scripted failures remain, and
/// hang (sleep [`HANG`]) while the matching flag is set.
#[derive(Default)]
struct FakeStorage {
    calls: Mutex<Vec<Call>>,
    file_failures: Mutex<VecDeque<StorageError>>,
    store_failures: Mutex<VecDeque<StorageError>>,
    hang_files: AtomicBool,
    hang_stores: AtomicBool,
    hang_secondary: AtomicBool,
}

async fn hang_if(flag: &AtomicBool) {
    if flag.load(Ordering::SeqCst) {
        tokio::time::sleep(HANG).await;
    }
}

impl FakeStorage {
    fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    fn fail_files(&self, times: usize) {
        let mut failures = lock(&self.file_failures);
        for _ in 0..times {
            failures.push_back(StorageError::Transient("provider 500".to_owned()));
        }
    }

    fn fail_stores(&self, times: usize) {
        let mut failures = lock(&self.store_failures);
        for _ in 0..times {
            failures.push_back(StorageError::Rejected("provider 500".to_owned()));
        }
    }
}

#[async_trait]
impl FileStorage for FakeStorage {
    fn backend_for_provider(&self, provider_id: &str) -> Result<String, DomainError> {
        Ok(provider_id.to_owned())
    }

    async fn upload_file(
        &self,
        _target: &StorageTarget,
        _file: FileUpload,
    ) -> Result<String, StorageError> {
        Err(StorageError::Rejected("not used".to_owned()))
    }

    async fn delete_file(
        &self,
        _target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        lock(&self.calls).push(Call::File(file_id.to_owned()));
        hang_if(&self.hang_files).await;
        lock(&self.file_failures).pop_front().map_or(Ok(()), Err)
    }
}

#[async_trait]
impl VectorStores for FakeStorage {
    async fn create_vector_store(
        &self,
        _target: &StorageTarget,
        _chat_id: Uuid,
    ) -> Result<String, StorageError> {
        Err(StorageError::Rejected("not used".to_owned()))
    }

    async fn delete_vector_store(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        lock(&self.calls).push(Call::Store(vector_store_id.to_owned()));
        hang_if(&self.hang_stores).await;
        lock(&self.store_failures).pop_front().map_or(Ok(()), Err)
    }

    async fn add_file(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
        _file_id: &str,
        _attachment_id: Uuid,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        Err(StorageError::Rejected("not used".to_owned()))
    }

    async fn file_status(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
        _file_id: &str,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        Err(StorageError::Rejected("not used".to_owned()))
    }
}

#[async_trait]
impl SecondaryFileStorage for FakeStorage {
    async fn delete_secondary_file(
        &self,
        _tenant_id: Uuid,
        file_id: &str,
        _provider_kind: &str,
        upstream_alias: &str,
    ) -> Result<(), StorageError> {
        lock(&self.calls).push(Call::Secondary(
            file_id.to_owned(),
            upstream_alias.to_owned(),
        ));
        hang_if(&self.hang_secondary).await;
        Ok(())
    }
}

struct FixedAlias;

impl SecondaryAliasResolver for FixedAlias {
    fn upstream_alias(
        &self,
        _tenant_id: Uuid,
        provider_kind: &str,
        _provider_id: Option<&str>,
    ) -> Option<String> {
        (provider_kind == "anthropic").then(|| "api.anthropic.com".to_owned())
    }
}

struct Fx {
    store: TestStore,
    storage: Arc<FakeStorage>,
    handler: ChatCleanupHandler,
    chat_id: Uuid,
}

impl Fx {
    async fn new() -> Self {
        let store = TestStore::new().await;
        let storage = Arc::new(FakeStorage::default());
        let handler = ChatCleanupHandler::new(
            Arc::clone(&store.repo),
            storage.clone(),
            storage.clone(),
            MAX_ATTEMPTS,
            "mini_chat_test",
        );
        let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
        Self {
            store,
            storage,
            handler,
            chat_id,
        }
    }

    /// An attachment of the chat with `cleanup_status`.
    async fn attachment(&self, cleanup_status: Option<&str>) -> Uuid {
        let mut seed =
            AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "doc.txt");
        seed.cleanup_status = cleanup_status.map(str::to_owned);
        seed_attachment(&self.store, self.chat_id, TENANT_A, seed).await
    }

    async fn set(&self, id: Uuid, column: attachments::Column, value: Option<&str>) {
        attachments::Entity::update_many()
            .col_expr(column, Expr::value(value.map(str::to_owned)))
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&self.store.conn())
            .await
            .unwrap();
    }

    async fn delete_chat(&self) {
        assert!(
            chat_repo::mark_deleted(
                &self.store.conn(),
                self.store.ts(),
                &AccessScope::allow_all(),
                self.chat_id,
                at(0),
            )
            .await
            .unwrap()
        );
    }

    /// The chat's vector store row; `None` id leaves a creation placeholder.
    async fn vector_store(&self, vector_store_id: Option<&str>) {
        let scope = AccessScope::for_tenant(TENANT_A);
        let row_id = Uuid::new_v4();
        let conn = self.store.conn();
        let placeholder = NewVectorStorePlaceholder {
            id: row_id,
            tenant_id: TENANT_A,
            chat_id: self.chat_id,
            provider: "openai".to_owned(),
            now: at(0),
        };
        assert!(
            vector_store_repo::insert_placeholder(&conn, self.store.ts(), &scope, placeholder)
                .await
                .unwrap()
        );
        if let Some(id) = vector_store_id {
            assert!(
                vector_store_repo::set_vector_store_id(&conn, &scope, self.chat_id, row_id, id)
                    .await
                    .unwrap()
            );
        }
    }

    async fn store_row(&self) -> Option<VectorStoreRecord> {
        vector_store_repo::find(
            &self.store.conn(),
            &AccessScope::for_tenant(TENANT_A),
            self.chat_id,
        )
        .await
        .unwrap()
    }

    fn payload(&self) -> ChatCleanupPayload {
        ChatCleanupPayload {
            tenant_id: TENANT_A,
            chat_id: self.chat_id,
            system_request_id: Uuid::new_v4(),
            reason: ChatCleanupReason::ChatSoftDelete,
            chat_deleted_at: OffsetDateTime::now_utc(),
        }
    }

    async fn deliver(&self, attempt: u32) -> MessageResult {
        self.handler.process(&self.payload(), attempt).await
    }

    async fn file_of(&self, id: Uuid) -> String {
        attachment_row(&self.store, id)
            .await
            .provider_file_id
            .unwrap()
    }
}

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn reject_reason(result: MessageResult) -> String {
    match result {
        MessageResult::Reject(reason) => reason,
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ── Chat state ───────────────────────────────────────────────────────────

#[tokio::test]
async fn active_or_unknown_chat_is_rejected_without_provider_calls() {
    let fx = Fx::new().await;
    fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;

    let reason = reject_reason(fx.deliver(1).await);
    assert_eq!(reason, "chat is not soft-deleted");
    let unknown = ChatCleanupPayload {
        chat_id: Uuid::new_v4(),
        ..fx.payload()
    };
    assert_eq!(
        reject_reason(fx.handler.process(&unknown, 1).await),
        "chat is not soft-deleted"
    );
    assert!(fx.storage.calls().is_empty());
    assert!(fx.store_row().await.is_some());
}

#[tokio::test]
async fn malformed_payload_is_rejected() {
    let fx = Fx::new().await;
    let result = fx
        .handler
        .handle_message(&message(b"{}".to_vec(), 0), None)
        .await;
    assert!(reject_reason(result).starts_with("malformed chat cleanup payload"));
}

// ── Ordering and outcomes ────────────────────────────────────────────────

#[tokio::test]
async fn files_are_deleted_before_the_vector_store_and_rows_completed() {
    let fx = Fx::new().await;
    let first = fx.attachment(Some("pending")).await;
    let second = fx.attachment(Some("pending")).await;
    let no_file = fx.attachment(Some("pending")).await;
    fx.set(no_file, attachments::Column::ProviderFileId, None)
        .await;
    let already_done = fx.attachment(Some("done")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;

    let payload = serde_json::to_vec(&fx.payload()).unwrap();
    let result = fx.handler.handle_message(&message(payload, 0), None).await;

    assert!(is_ok(&result), "{result:?}");
    let calls = fx.storage.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    for id in [first, second] {
        assert!(
            calls[..2].contains(&Call::File(fx.file_of(id).await)),
            "{calls:?}"
        );
    }
    assert_eq!(
        calls[2],
        Call::Store("vs_1".to_owned()),
        "the store goes last"
    );
    for id in [first, second, no_file] {
        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"), "{row:?}");
        assert_eq!(row.cleanup_attempts, 0);
        assert_eq!(row.last_cleanup_error, None);
        assert!(row.cleanup_updated_at.is_some());
    }
    assert_eq!(
        attachment_row(&fx.store, already_done)
            .await
            .cleanup_updated_at,
        None,
        "terminal rows are not touched"
    );
    assert_eq!(fx.store_row().await, None);

    // A replay finds nothing left to do.
    assert!(is_ok(&fx.deliver(2).await));
    assert_eq!(fx.storage.calls().len(), 3);
}

#[tokio::test]
async fn chat_without_vector_store_completes_after_the_files() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.delete_chat().await;

    assert!(is_ok(&fx.deliver(1).await));
    assert_eq!(fx.storage.calls(), [Call::File(fx.file_of(id).await)]);
}

#[tokio::test]
async fn failed_file_delete_is_recorded_and_retried_before_the_store_is_touched() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.fail_files(5);

    let first = fx.deliver(1).await;

    assert!(is_retry(&first), "{first:?}");
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert!(
        row.last_cleanup_error
            .as_deref()
            .unwrap()
            .contains("provider 500")
    );
    assert!(row.cleanup_updated_at.is_some());
    assert!(fx.store_row().await.is_some());
    assert!(
        !fx.storage.calls().contains(&Call::Store("vs_1".to_owned())),
        "the store waits for every attachment to be terminal"
    );

    // The attempt that reaches the limit marks the attachment failed; the
    // failed attachment does not block the vector-store delete.
    let second = fx.deliver(2).await;

    assert!(is_ok(&second), "{second:?}");
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, i32::try_from(MAX_ATTEMPTS).unwrap());
    let file = fx.file_of(id).await;
    assert_eq!(
        fx.storage.calls(),
        [
            Call::File(file.clone()),
            Call::File(file),
            Call::Store("vs_1".to_owned()),
        ]
    );
    assert_eq!(fx.store_row().await, None);
}

#[tokio::test]
async fn failed_store_delete_retries_then_rejects_at_the_limit_and_keeps_the_row() {
    let fx = Fx::new().await;
    fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.fail_stores(5);

    let first = fx
        .handler
        .handle_message(
            &message(serde_json::to_vec(&fx.payload()).unwrap(), 0),
            None,
        )
        .await;
    assert!(is_retry(&first), "{first:?}");
    let last = fx
        .handler
        .handle_message(
            &message(serde_json::to_vec(&fx.payload()).unwrap(), 1),
            None,
        )
        .await;

    assert_eq!(
        reject_reason(last),
        format!("vector store delete: max attempts ({MAX_ATTEMPTS}) reached")
    );
    let stores = fx
        .storage
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Store(_)))
        .count();
    assert_eq!(stores, 2, "every delivery counts toward the limit");
    let row = fx.store_row().await.unwrap();
    assert_eq!(row.vector_store_id.as_deref(), Some("vs_1"));
}

#[tokio::test]
async fn placeholder_row_is_removed_without_a_provider_call() {
    let fx = Fx::new().await;
    fx.vector_store(None).await;
    fx.delete_chat().await;

    assert!(is_ok(&fx.deliver(1).await));
    assert!(fx.storage.calls().is_empty());
    assert_eq!(fx.store_row().await, None);
}

#[tokio::test]
async fn secondary_copy_is_deleted_when_the_alias_resolves_and_skipped_otherwise() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.set(
        id,
        attachments::Column::SecondaryFileId,
        Some("anthropic-1"),
    )
    .await;
    fx.set(
        id,
        attachments::Column::SecondaryProviderKind,
        Some("anthropic"),
    )
    .await;
    fx.delete_chat().await;

    // Without secondary storage the copy is skipped; the primary is done.
    assert!(is_ok(&fx.deliver(1).await));
    assert_eq!(fx.storage.calls(), [Call::File(fx.file_of(id).await)]);

    fx.set(id, attachments::Column::CleanupStatus, Some("pending"))
        .await;
    let handler = ChatCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        fx.storage.clone(),
        fx.storage.clone(),
        MAX_ATTEMPTS,
        "mini_chat_test",
    )
    .with_secondary(fx.storage.clone(), Arc::new(FixedAlias));
    assert!(is_ok(&handler.process(&fx.payload(), 1).await));
    assert_eq!(
        fx.storage.calls().last(),
        Some(&Call::Secondary(
            "anthropic-1".to_owned(),
            "api.anthropic.com".to_owned()
        ))
    );
}

// ── Provider 404 through the real storage client ─────────────────────────

#[tokio::test]
async fn provider_not_found_is_success_for_the_file_and_the_vector_store() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_404")).await;
    fx.delete_chat().await;
    let gateway = Arc::new(FakeGateway::default());
    for _ in 0..2 {
        gateway.push_proxy(Ok(json_response(
            404,
            &json!({ "error": { "message": "not found" } }),
            &[],
            None,
        )));
    }
    let providers: BTreeMap<String, ProviderEntry> = BTreeMap::from([(
        "openai".to_owned(),
        serde_json::from_value(json!({
            "kind": "openai_responses",
            "host": "api.openai.com",
            "upstream_alias": "api.openai.com",
            "storage_kind": "openai",
        }))
        .unwrap(),
    )]);
    let storage = Arc::new(ProviderStorage::new(
        gateway.clone(),
        Arc::new(StaticSystemContext::default()),
        &providers,
    ));
    let handler = ChatCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        storage.clone(),
        storage,
        MAX_ATTEMPTS,
        "mini_chat_test",
    );

    let result = handler.process(&fx.payload(), 1).await;

    assert!(is_ok(&result), "{result:?}");
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
    assert_eq!(row.last_cleanup_error, None);
    assert_eq!(fx.store_row().await, None);
    let proxied = lock(&gateway.proxied);
    assert_eq!(proxied.len(), 2);
    assert!(
        proxied[0]
            .uri
            .ends_with(&format!("/files/{}", row.provider_file_id.unwrap()))
    );
    assert!(proxied[1].uri.ends_with("/vector_stores/vs_404"));
}

// ── Provider call timeouts and the delivery budget ───────────────────────

impl Fx {
    /// A handler over the fake storage with the given delivery budget and
    /// [`CALL_TIMEOUT`].
    fn timed_handler(&self, work_budget: Duration) -> ChatCleanupHandler {
        ChatCleanupHandler::new(
            Arc::clone(&self.store.repo),
            self.storage.clone(),
            self.storage.clone(),
            MAX_ATTEMPTS,
            "mini_chat_test",
        )
        .with_timing(work_budget, CALL_TIMEOUT)
    }
}

fn file_calls(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|call| matches!(call, Call::File(_)))
        .count()
}

#[tokio::test]
async fn default_budget_follows_the_lease_and_bounds_every_call() {
    let fx = Fx::new().await;
    let lease = LeaseConfig::default();
    assert_eq!(
        (lease.duration, lease.headroom),
        (Duration::from_secs(30), Duration::from_secs(2))
    );
    assert_eq!(fx.handler.work_budget, Duration::from_secs(26));
    assert_eq!(fx.handler.call_timeout, PROVIDER_CALL_TIMEOUT);
    assert!(PROVIDER_CALL_TIMEOUT * 2 <= fx.handler.work_budget);

    let short = fx.timed_handler(Duration::ZERO).with_lease(LeaseConfig {
        duration: Duration::from_secs(10),
        headroom: Duration::from_secs(2),
    });
    assert_eq!(short.work_budget, Duration::from_secs(6));
    assert_eq!(short.call_timeout, Duration::from_secs(3));
}

#[tokio::test]
async fn hanging_file_delete_is_a_failed_attempt_until_the_attachment_fails() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.hang_files.store(true, Ordering::SeqCst);
    let handler = fx.timed_handler(Duration::from_secs(5));

    let started = Instant::now();
    let first = handler.process(&fx.payload(), 1).await;

    assert!(is_retry(&first), "{first:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by the call timeout"
    );
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert!(
        row.last_cleanup_error
            .as_deref()
            .unwrap()
            .contains("timed out"),
        "{row:?}"
    );

    let second = handler.process(&fx.payload(), 2).await;

    assert!(is_ok(&second), "{second:?}");
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, i32::try_from(MAX_ATTEMPTS).unwrap());
    assert_eq!(
        fx.store_row().await,
        None,
        "the store is deleted after the failure"
    );
}

#[tokio::test]
async fn hanging_store_delete_retries_then_rejects_at_the_limit() {
    let fx = Fx::new().await;
    fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.hang_stores.store(true, Ordering::SeqCst);
    let handler = fx.timed_handler(Duration::from_secs(5));

    let first = handler.process(&fx.payload(), 1).await;
    assert!(is_retry(&first), "{first:?}");
    let last = handler.process(&fx.payload(), MAX_ATTEMPTS).await;

    assert_eq!(
        reject_reason(last),
        format!("vector store delete: max attempts ({MAX_ATTEMPTS}) reached")
    );
    let row = fx.store_row().await.unwrap();
    assert_eq!(row.vector_store_id.as_deref(), Some("vs_1"));
}

#[tokio::test]
async fn hanging_secondary_delete_does_not_block_the_primary_outcome() {
    let fx = Fx::new().await;
    let id = fx.attachment(Some("pending")).await;
    fx.set(
        id,
        attachments::Column::SecondaryFileId,
        Some("anthropic-1"),
    )
    .await;
    fx.set(
        id,
        attachments::Column::SecondaryProviderKind,
        Some("anthropic"),
    )
    .await;
    fx.delete_chat().await;
    fx.storage.hang_secondary.store(true, Ordering::SeqCst);
    let handler = fx
        .timed_handler(Duration::from_secs(5))
        .with_secondary(fx.storage.clone(), Arc::new(FixedAlias));

    let result = handler.process(&fx.payload(), 1).await;

    assert!(is_ok(&result), "{result:?}");
    let row = attachment_row(&fx.store, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn spent_budget_stops_starting_provider_calls_and_retries() {
    let fx = Fx::new().await;
    let ids = [
        fx.attachment(Some("pending")).await,
        fx.attachment(Some("pending")).await,
        fx.attachment(Some("pending")).await,
    ];
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.hang_files.store(true, Ordering::SeqCst);
    // Room for at most two call timeouts.
    let handler = fx.timed_handler(CALL_TIMEOUT * 5 / 2);

    let result = handler.process(&fx.payload(), 1).await;

    assert!(is_retry(&result), "{result:?}");
    let calls = fx.storage.calls();
    let attempted = file_calls(&calls);
    assert!((1..ids.len()).contains(&attempted), "{calls:?}");
    assert!(
        !calls.iter().any(|call| matches!(call, Call::Store(_))),
        "{calls:?}"
    );
    let mut recorded = 0;
    for id in ids {
        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        recorded += row.cleanup_attempts;
    }
    assert_eq!(
        usize::try_from(recorded).unwrap(),
        attempted,
        "every started call records its outcome"
    );

    // A spent budget before the store delete retries without a store call.
    fx.storage.hang_files.store(false, Ordering::SeqCst);
    for id in ids {
        fx.set(id, attachments::Column::CleanupStatus, Some("done"))
            .await;
    }
    let no_room = fx.timed_handler(CALL_TIMEOUT / 2);
    assert!(is_retry(&no_room.process(&fx.payload(), 1).await));
    assert!(fx.store_row().await.is_some());
    assert!(
        !fx.storage
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Store(_)))
    );
}

// ── Batch lease deadline ─────────────────────────────────────────────────

#[tokio::test]
async fn deadline_is_the_own_budget_capped_by_the_batch_lease_minus_the_db_reserve() {
    let fx = Fx::new().await;
    let budget = Duration::from_secs(5);
    let handler = fx.timed_handler(budget);

    // Outside a lease: the own budget.
    let before = Instant::now();
    assert!(handler.deadline(None).at >= before + budget);

    // A distant lease end does not extend the own budget.
    let far = Instant::now() + Duration::from_secs(60);
    let before = Instant::now();
    let at = handler.deadline(Some(far)).at;
    assert!(at >= before + budget && at < far - DB_RESERVE);

    // A near lease end caps it, keeping the DB reserve free.
    let near = Instant::now() + DB_RESERVE + Duration::from_millis(100);
    assert_eq!(handler.deadline(Some(near)).at, near - DB_RESERVE);

    // A lease ending inside the DB reserve leaves no room for a call.
    let spent = Instant::now() + DB_RESERVE / 2;
    assert!(!handler.deadline(Some(spent)).allows_call());
}

/// A second soft-deleted chat of `TENANT_A` with one `pending` attachment.
async fn second_deleted_chat(fx: &Fx) -> (Uuid, Uuid) {
    let chat_id = seed_chat(&fx.store, TENANT_A, USER_A1, None).await;
    let mut seed =
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "doc.txt");
    seed.cleanup_status = Some("pending".to_owned());
    let attachment = seed_attachment(&fx.store, chat_id, TENANT_A, seed).await;
    assert!(
        chat_repo::mark_deleted(
            &fx.store.conn(),
            fx.store.ts(),
            &AccessScope::allow_all(),
            chat_id,
            at(0),
        )
        .await
        .unwrap()
    );
    (chat_id, attachment)
}

#[tokio::test]
async fn later_message_of_a_batch_starts_no_call_past_the_lease() {
    let fx = Fx::new().await;
    // First message: its file delete hangs until the call timeout, then the
    // attempt is terminal (`failed`) and the chat completes (`Ok`).
    let first = fx.attachment(Some("pending")).await;
    attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupAttempts,
            Expr::value(i32::try_from(MAX_ATTEMPTS).unwrap() - 1),
        )
        .filter(attachments::Column::Id.eq(first))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&fx.store.conn())
        .await
        .unwrap();
    fx.delete_chat().await;
    let (second_chat, second) = second_deleted_chat(&fx).await;
    fx.storage.hang_files.store(true, Ordering::SeqCst);
    // The own budget would fit many calls per message; the batch lease
    // leaves room for one call timeout and a half after the DB reserve.
    let call_timeout = Duration::from_millis(200);
    let handler = ChatCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        fx.storage.clone(),
        fx.storage.clone(),
        MAX_ATTEMPTS,
        "mini_chat_test",
    )
    .with_timing(Duration::from_secs(5), call_timeout);
    let lease_end = Instant::now() + DB_RESERVE + call_timeout * 3 / 2;
    let batch = [
        message(serde_json::to_vec(&fx.payload()).unwrap(), 0),
        message(
            serde_json::to_vec(&ChatCleanupPayload {
                chat_id: second_chat,
                ..fx.payload()
            })
            .unwrap(),
            0,
        ),
    ];

    let first_result = handler.handle_message(&batch[0], Some(lease_end)).await;
    let second_result = handler.handle_message(&batch[1], Some(lease_end)).await;

    assert!(is_ok(&first_result), "{first_result:?}");
    assert_eq!(
        attachment_row(&fx.store, first)
            .await
            .cleanup_status
            .as_deref(),
        Some("failed")
    );
    assert!(is_retry(&second_result), "{second_result:?}");
    assert!(
        Instant::now() + DB_RESERVE / 2 < lease_end,
        "the batch ends before the lease cancel point"
    );
    let calls = fx.storage.calls();
    assert_eq!(file_calls(&calls), 1, "{calls:?}");
    let row = attachment_row(&fx.store, second).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 0, "no call started, none recorded");
}

// ── Failed-attachment vector store metric ────────────────────────────────

const FAILED_METRIC: &str = "mini_chat_test_cleanup_vector_store_with_failed_attachments";

/// An in-memory meter for one test.
struct Meters {
    provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    exporter: opentelemetry_sdk::metrics::InMemoryMetricExporter,
}

impl Meters {
    fn new() -> Self {
        let exporter = opentelemetry_sdk::metrics::InMemoryMetricExporter::default();
        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_reader(
                opentelemetry_sdk::metrics::PeriodicReader::builder(exporter.clone()).build(),
            )
            .build();
        Self { provider, exporter }
    }

    fn handler(&self, fx: &Fx) -> ChatCleanupHandler {
        use opentelemetry::metrics::MeterProvider as _;
        ChatCleanupHandler::new(
            Arc::clone(&fx.store.repo),
            fx.storage.clone(),
            fx.storage.clone(),
            MAX_ATTEMPTS,
            "mini_chat_test",
        )
        .with_meter(&self.provider.meter("mini-chat-test"), "mini_chat_test")
    }

    /// The cumulative value of the counter `name` (0 when never recorded).
    fn counter(&self, name: &str) -> u64 {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        self.provider.force_flush().unwrap();
        let mut value = 0;
        for resource in self.exporter.get_finished_metrics().unwrap() {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                    {
                        value = sum
                            .data_points()
                            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                            .sum();
                    }
                }
            }
        }
        value
    }
}

#[tokio::test]
async fn failed_attachment_without_a_vector_store_is_not_counted() {
    let fx = Fx::new().await;
    fx.attachment(Some("failed")).await;
    fx.delete_chat().await;
    let meters = Meters::new();
    let handler = meters.handler(&fx);

    assert!(is_ok(&handler.process(&fx.payload(), 1).await));
    assert!(is_ok(&handler.process(&fx.payload(), 2).await));

    assert_eq!(meters.counter(FAILED_METRIC), 0);
}

#[tokio::test]
async fn failed_attachment_with_a_placeholder_or_no_budget_is_not_counted() {
    let fx = Fx::new().await;
    fx.attachment(Some("failed")).await;
    fx.vector_store(None).await;
    fx.delete_chat().await;
    let meters = Meters::new();

    assert!(is_ok(&meters.handler(&fx).process(&fx.payload(), 1).await));
    assert_eq!(fx.store_row().await, None);
    assert_eq!(meters.counter(FAILED_METRIC), 0, "placeholder: no delete");

    fx.vector_store(Some("vs_1")).await;
    let no_room = meters
        .handler(&fx)
        .with_timing(Duration::ZERO, CALL_TIMEOUT);
    assert!(is_retry(&no_room.process(&fx.payload(), 1).await));
    assert_eq!(meters.counter(FAILED_METRIC), 0, "no delete executed");
}

#[tokio::test]
async fn failed_attachment_is_counted_once_per_executed_store_delete() {
    let fx = Fx::new().await;
    fx.attachment(Some("failed")).await;
    fx.attachment(Some("done")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    fx.storage.fail_stores(1);
    let meters = Meters::new();
    let handler = meters.handler(&fx);

    let first = handler.process(&fx.payload(), 1).await;
    assert!(is_retry(&first), "{first:?}");
    assert_eq!(meters.counter(FAILED_METRIC), 1);

    let second = handler.process(&fx.payload(), 2).await;
    assert!(is_ok(&second), "{second:?}");
    assert_eq!(meters.counter(FAILED_METRIC), 2);
    assert_eq!(fx.store_row().await, None);

    // Nothing left to delete: a replay counts nothing.
    assert!(is_ok(&handler.process(&fx.payload(), 3).await));
    assert_eq!(meters.counter(FAILED_METRIC), 2);
}

#[tokio::test]
async fn store_delete_without_failed_attachments_is_not_counted() {
    let fx = Fx::new().await;
    fx.attachment(Some("pending")).await;
    fx.vector_store(Some("vs_1")).await;
    fx.delete_chat().await;
    let meters = Meters::new();

    assert!(is_ok(&meters.handler(&fx).process(&fx.payload(), 1).await));
    assert_eq!(fx.store_row().await, None);
    assert_eq!(meters.counter(FAILED_METRIC), 0);
}
