#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use mini_chat_sdk::{
    MiniChatAuditEvent, PolicySnapshot, PublishError, TurnMutationAuditEvent, UsageEvent,
    UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert, secure_update_with_scope};
use uuid::Uuid;

use super::CleanupDeps;
use super::attachment_cleanup::AttachmentCleanupHandler;
use super::audit_handler::AuditHandler;
use super::chat_cleanup::ChatCleanupHandler;
use super::payloads::{AttachmentCleanupPayload, ChatCleanupPayload};
use super::usage_handler::UsageHandler;
use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    AuditDelivery, AuditSink, IndexStatus, PolicyProvider, RagStorage, StorageError,
};
use crate::domain::time::db_now;
use crate::infra::db::entities::{attachment, chat, chat_vector_store};
use crate::infra::db::repos::{attachment_repo, chat_repo};
use crate::infra::llm::{ProviderResolver, ResolvedStorage};
use crate::test_support::{ctx_for, seed_attachment, seed_chat, test_provider};

// ── Messages ─────────────────────────────────────────────────────────────────

fn msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "test".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn json_msg<T: serde::Serialize>(value: &T, attempts: i16) -> OutboxMessage {
    msg(serde_json::to_vec(value).unwrap(), attempts)
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}

fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}

fn reject_reason(r: MessageResult) -> String {
    match r {
        MessageResult::Reject(reason) => reason,
        other => panic!("expected Reject, got {other:?}"),
    }
}

fn ts() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

// ── Usage ────────────────────────────────────────────────────────────────────

/// Policy provider whose `publish_usage` pops scripted results.
struct FakePublisher {
    results: Mutex<VecDeque<Result<(), PublishError>>>,
    published: Mutex<Vec<UsageEvent>>,
}

impl FakePublisher {
    fn new(results: Vec<Result<(), PublishError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            published: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl PolicyProvider for FakePublisher {
    async fn current(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        unreachable!("usage handler only publishes")
    }

    async fn snapshot(
        &self,
        _user_id: Uuid,
        _version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        unreachable!("usage handler only publishes")
    }

    async fn user_limits(&self, _user_id: Uuid, _version: u64) -> Result<UserLimits, DomainError> {
        unreachable!("usage handler only publishes")
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().unwrap().push(ev);
        self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}

fn usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 7,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: ts(),
        requester_type: "user".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

#[tokio::test]
async fn usage_handler_permanent_rejects_transient_retries() {
    let policy = Arc::new(FakePublisher::new(vec![
        Err(PublishError::Transient("plugin down".to_owned())),
        Err(PublishError::Permanent("bad event".to_owned())),
        Ok(()),
    ]));
    let handler = UsageHandler::new(policy.clone());
    let ev = usage_event();

    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));
    let reason = reject_reason(handler.handle(&json_msg(&ev, 1)).await);
    assert!(reason.contains("bad event"), "{reason}");
    assert!(is_ok(&handler.handle(&json_msg(&ev, 0)).await));
    assert_eq!(
        policy.published.lock().unwrap().as_slice(),
        &[ev.clone(), ev.clone(), ev]
    );

    // A payload that does not deserialize is rejected without a publish.
    let reason = reject_reason(handler.handle(&msg(b"{not json".to_vec(), 0)).await);
    assert!(reason.contains("usage"), "{reason}");
    assert_eq!(policy.published.lock().unwrap().len(), 3);
}

// ── Audit ────────────────────────────────────────────────────────────────────

struct FakeSink {
    outcome: AuditDelivery,
    calls: Mutex<u32>,
}

impl FakeSink {
    fn new(outcome: AuditDelivery) -> Self {
        Self {
            outcome,
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl AuditSink for FakeSink {
    async fn deliver(&self, _ev: MiniChatAuditEvent) -> AuditDelivery {
        *self.calls.lock().unwrap() += 1;
        self.outcome.clone()
    }
}

fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: "turn_delete".to_owned(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::new_v4()),
        timestamp: ts(),
    })
}

#[tokio::test]
async fn audit_handler_rejects_corrupt_payload_even_without_plugin() {
    let sink = Arc::new(FakeSink::new(AuditDelivery::NoPlugin));
    let handler = AuditHandler::new(sink.clone());

    let reason = reject_reason(
        handler
            .handle(&msg(br#"{"kind":"nope"}"#.to_vec(), 0))
            .await,
    );
    assert!(reason.contains("audit"), "{reason}");
    assert_eq!(sink.calls(), 0, "the payload is checked before the plugin");

    // A valid event without a plugin is acknowledged (dropped).
    assert!(is_ok(&handler.handle(&json_msg(&audit_event(), 0)).await));
    assert_eq!(sink.calls(), 1);

    let delivered = AuditHandler::new(Arc::new(FakeSink::new(AuditDelivery::Delivered)));
    assert!(is_ok(&delivered.handle(&json_msg(&audit_event(), 0)).await));
    let refused = AuditHandler::new(Arc::new(FakeSink::new(AuditDelivery::Reject(
        "permanent".to_owned(),
    ))));
    let reason = reject_reason(refused.handle(&json_msg(&audit_event(), 0)).await);
    assert!(reason.contains("permanent"), "{reason}");
}

#[tokio::test]
async fn audit_handler_120th_attempt_rejects() {
    let handler = AuditHandler::new(Arc::new(FakeSink::new(AuditDelivery::Retry(
        "plugin unavailable".to_owned(),
    ))));
    let ev = audit_event();
    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));
    // `attempts` counts retries already taken: 118 -> 119th delivery.
    assert!(is_retry(&handler.handle(&json_msg(&ev, 118)).await));
    let reason = reject_reason(handler.handle(&json_msg(&ev, 119)).await);
    assert!(reason.contains("120"), "{reason}");
    assert!(reason.contains("plugin unavailable"), "{reason}");
}

// ── Cleanup fixture ──────────────────────────────────────────────────────────

const BACKEND: &str = "openai";
/// The alias of the fixture tenant's override; the base alias is `api.example.com`.
const TENANT_ALIAS: &str = "tenant-upstream";

/// [`RagStorage`] fake for deletes: records calls in order; delete results
/// are popped from the scripts, then `Ok`.
#[derive(Default)]
struct FakeStore {
    calls: Mutex<Vec<String>>,
    /// The alias of the storage every delete was addressed to.
    aliases: Mutex<Vec<String>>,
    file_results: Mutex<VecDeque<Result<(), StorageError>>>,
    vs_results: Mutex<VecDeque<Result<(), StorageError>>>,
    /// When set, `delete_file` first starts a transaction on this database
    /// that holds the write lock until the paired sender fires (makes the
    /// handler's next write fail with `SQLITE_BUSY`).
    write_lock: Mutex<Option<WriteLock>>,
}

struct WriteLock {
    db: Arc<DBProvider<DomainError>>,
    chat: chat::Model,
    release: tokio::sync::oneshot::Receiver<()>,
    holder: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl WriteLock {
    /// Starts the lock-holding transaction; returns once the lock is held.
    async fn take(self) {
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let Self {
            db,
            chat,
            release,
            holder,
        } = self;
        let handle = tokio::spawn(async move {
            db.transaction(move |tx| {
                Box::pin(async move {
                    chat_repo::touch_updated_at(tx, chat.tenant_id, chat.id, db_now()).await?;
                    locked_tx.send(()).unwrap();
                    release.await.ok();
                    Ok(())
                })
            })
            .await
            .unwrap();
        });
        *holder.lock().unwrap() = Some(handle);
        locked_rx.await.unwrap();
    }
}

impl FakeStore {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn aliases(&self) -> Vec<String> {
        self.aliases.lock().unwrap().clone()
    }

    fn script_files(&self, results: Vec<Result<(), StorageError>>) {
        self.file_results.lock().unwrap().extend(results);
    }

    fn script_vs(&self, results: Vec<Result<(), StorageError>>) {
        self.vs_results.lock().unwrap().extend(results);
    }
}

#[async_trait]
impl RagStorage for FakeStore {
    async fn upload_file(
        &self,
        _st: &ResolvedStorage,
        _filename: &str,
        _content_type: &str,
        _bytes: Bytes,
    ) -> Result<String, StorageError> {
        unreachable!("cleanup never uploads")
    }

    async fn delete_file(&self, st: &ResolvedStorage, file_id: &str) -> Result<(), StorageError> {
        assert_eq!(st.backend_label, BACKEND);
        self.aliases.lock().unwrap().push(st.alias.clone());
        self.calls
            .lock()
            .unwrap()
            .push(format!("delete_file:{file_id}"));
        let lock = self.write_lock.lock().unwrap().take();
        if let Some(lock) = lock {
            lock.take().await;
        }
        self.file_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn create_vector_store(
        &self,
        _st: &ResolvedStorage,
        _name: &str,
    ) -> Result<String, StorageError> {
        unreachable!("cleanup never creates stores")
    }

    async fn add_file_to_vector_store(
        &self,
        _st: &ResolvedStorage,
        _vs: &str,
        _file_id: &str,
        _attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        unreachable!("cleanup never indexes")
    }

    async fn vector_store_file_status(
        &self,
        _st: &ResolvedStorage,
        _vs: &str,
        _file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        unreachable!("cleanup never polls")
    }

    async fn delete_vector_store(
        &self,
        st: &ResolvedStorage,
        vs: &str,
    ) -> Result<(), StorageError> {
        assert_eq!(st.backend_label, BACKEND);
        self.aliases.lock().unwrap().push(st.alias.clone());
        self.calls.lock().unwrap().push(format!("delete_vs:{vs}"));
        self.vs_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }
}

struct Fx {
    db: Arc<DBProvider<DomainError>>,
    store: Arc<FakeStore>,
    deps: CleanupDeps,
    chat: chat::Model,
}

async fn fx(max_attempts: u32) -> Fx {
    fx_on(test_provider().await, max_attempts).await
}

async fn fx_on(db: Arc<DBProvider<DomainError>>, max_attempts: u32) -> Fx {
    let tenant_id = Uuid::new_v4();
    let mut cfg = MiniChatConfig {
        providers: serde_json::from_value(json!({
            BACKEND: {
                "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai",
                "tenant_overrides": {tenant_id.to_string(): {"upstream_alias": TENANT_ALIAS}}
            }
        }))
        .unwrap(),
        ..MiniChatConfig::default()
    };
    cfg.fill_upstream_aliases();
    let store = Arc::new(FakeStore::default());
    let deps = CleanupDeps {
        db: Arc::clone(&db),
        resolver: Arc::new(ProviderResolver::new(&cfg)),
        storage: store.clone(),
        max_attempts,
    };
    let ctx = ctx_for(tenant_id, Uuid::new_v4());
    let chat = seed_chat(&db, &ctx, None, db_now()).await;
    Fx {
        db,
        store,
        deps,
        chat,
    }
}

impl Fx {
    /// An attachment with `provider_file_id = file_id` (`None` = never
    /// uploaded), soft-deleted and `cleanup_status = 'pending'` (attachment
    /// delete path) when `deleted`.
    async fn attachment(
        &self,
        kind: &str,
        file_id: Option<&str>,
        deleted: bool,
    ) -> attachment::Model {
        let row = seed_attachment(&self.db, &self.chat, kind, "ready", None).await;
        let id = row.id;
        let mut am = row.into_active_model();
        am.provider_file_id = Set(file_id.map(str::to_owned));
        if deleted {
            am.deleted_at = Set(Some(db_now()));
            am.cleanup_status = Set(Some("pending".to_owned()));
            am.cleanup_updated_at = Set(Some(db_now()));
        }
        let conn = self.db.conn().unwrap();
        secure_update_with_scope::<attachment::Entity>(am, &AccessScope::allow_all(), id, &conn)
            .await
            .unwrap()
    }

    async fn row(&self, id: Uuid) -> attachment::Model {
        let conn = self.db.conn().unwrap();
        attachment::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .expect("attachment row")
    }

    /// Chat delete as the chat service does it: soft-delete + mark the live
    /// attachments `pending`.
    async fn delete_chat(&self) {
        let conn = self.db.conn().unwrap();
        let now = db_now();
        chat_repo::soft_delete(&conn, &AccessScope::allow_all(), self.chat.id, now)
            .await
            .unwrap();
        attachment_repo::mark_chat_cleanup_pending(&conn, self.chat.tenant_id, self.chat.id, now)
            .await
            .unwrap();
    }

    async fn insert_vector_store(&self, vs: &str) {
        let conn = self.db.conn().unwrap();
        secure_insert::<chat_vector_store::Entity>(
            chat_vector_store::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.chat.tenant_id),
                chat_id: Set(self.chat.id),
                vector_store_id: Set(Some(vs.to_owned())),
                provider: Set(BACKEND.to_owned()),
                file_count: Set(0),
                created_at: Set(db_now()),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    async fn insert_placeholder(&self) {
        let conn = self.db.conn().unwrap();
        crate::infra::db::repos::vector_store_repo::insert_placeholder(
            &conn,
            self.chat.tenant_id,
            self.chat.id,
            BACKEND,
            db_now(),
        )
        .await
        .unwrap();
    }

    async fn vector_stores(&self) -> Vec<chat_vector_store::Model> {
        let conn = self.db.conn().unwrap();
        chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    fn attachment_payload(a: &attachment::Model) -> AttachmentCleanupPayload {
        AttachmentCleanupPayload {
            event_type: "attachment_deleted".to_owned(),
            tenant_id: a.tenant_id,
            chat_id: a.chat_id,
            attachment_id: a.id,
            provider_file_id: a.provider_file_id.clone(),
            vector_store_id: None,
            storage_backend: a.storage_backend.clone(),
            attachment_kind: a.attachment_kind.clone(),
            deleted_at: db_now(),
            secondary_ref: None,
        }
    }

    fn chat_payload(&self) -> ChatCleanupPayload {
        ChatCleanupPayload {
            tenant_id: self.chat.tenant_id,
            chat_id: self.chat.id,
            system_request_id: Uuid::new_v4(),
            reason: "chat_soft_delete".to_owned(),
            chat_deleted_at: db_now(),
        }
    }
}

// ── Attachment cleanup ───────────────────────────────────────────────────────

#[tokio::test]
async fn attachment_cleanup_deletes_file_and_marks_done() {
    let fx = fx(5).await;
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), true).await;
    let before = fx.row(a.id).await.cleanup_updated_at.unwrap();

    let res = handler
        .handle(&json_msg(&Fx::attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&res), "{res:?}");
    assert_eq!(fx.store.calls(), vec!["delete_file:file-a"]);
    // The tenant's override alias, not the entry's base alias.
    assert_eq!(fx.store.aliases(), vec![TENANT_ALIAS]);
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
    assert!(row.cleanup_updated_at.unwrap() > before);

    // Redelivery of a finished cleanup: no provider call.
    assert!(is_ok(
        &handler
            .handle(&json_msg(&Fx::attachment_payload(&a), 0))
            .await
    ));
    assert_eq!(fx.store.calls().len(), 1);

    // Never uploaded: done without a provider call.
    let never = fx.attachment("image", None, true).await;
    assert!(is_ok(
        &handler
            .handle(&json_msg(&Fx::attachment_payload(&never), 0))
            .await
    ));
    assert_eq!(
        fx.row(never.id).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(fx.store.calls().len(), 1);

    // Malformed payload.
    let reason = reject_reason(handler.handle(&msg(b"[]".to_vec(), 0)).await);
    assert!(reason.contains("payload"), "{reason}");
}

#[tokio::test]
async fn attachment_cleanup_skips_when_chat_deleted() {
    let fx = fx(5).await;
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), true).await;
    fx.delete_chat().await;

    let res = handler
        .handle(&json_msg(&Fx::attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&res), "{res:?}");
    assert!(fx.store.calls().is_empty());
    let row = fx.row(a.id).await;
    assert_eq!(
        row.cleanup_status.as_deref(),
        Some("pending"),
        "owned by chat cleanup"
    );
    assert_eq!(row.cleanup_attempts, 0);
}

#[tokio::test]
async fn attachment_cleanup_404_is_success() {
    let fx = fx(5).await;
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("image", Some("file-gone"), true).await;
    fx.store.script_files(vec![Err(StorageError::NotFound)]);

    let res = handler
        .handle(&json_msg(&Fx::attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&res), "{res:?}");
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
    assert_eq!(row.last_cleanup_error, None);
}

#[tokio::test]
async fn attachment_cleanup_failure_increments_then_fails_at_max() {
    let fx = fx(3).await;
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), true).await;
    fx.store.script_files(vec![
        Err(StorageError::Transient("HTTP 503".to_owned())),
        Err(StorageError::Failed("HTTP 400".to_owned())),
        Err(StorageError::Transient("HTTP 500".to_owned())),
    ]);
    let payload = Fx::attachment_payload(&a);

    assert!(is_retry(&handler.handle(&json_msg(&payload, 0)).await));
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert!(row.last_cleanup_error.as_deref().unwrap().contains("503"));
    let first_update = row.cleanup_updated_at.unwrap();

    assert!(is_retry(&handler.handle(&json_msg(&payload, 1)).await));
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 2);
    assert!(row.last_cleanup_error.as_deref().unwrap().contains("400"));
    assert!(row.cleanup_updated_at.unwrap() > first_update);

    let reason = reject_reason(handler.handle(&json_msg(&payload, 2)).await);
    assert!(reason.contains("max attempts (3)"), "{reason}");
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 3);
    assert!(row.last_cleanup_error.as_deref().unwrap().contains("500"));
    assert_eq!(fx.store.calls().len(), 3);

    // Terminal: a replay does not call the provider again.
    assert!(is_ok(&handler.handle(&json_msg(&payload, 0)).await));
    assert_eq!(fx.store.calls().len(), 3);
}

// ── Chat cleanup ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_cleanup_rejects_active_chat() {
    let fx = fx(5).await;
    let handler = ChatCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), true).await;
    fx.insert_vector_store("vs-1").await;

    let reason = reject_reason(handler.handle(&json_msg(&fx.chat_payload(), 0)).await);
    assert_eq!(reason, "chat is not soft-deleted");
    assert!(fx.store.calls().is_empty());
    assert_eq!(
        fx.row(a.id).await.cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(fx.vector_stores().await.len(), 1);

    let reason = reject_reason(handler.handle(&msg(b"{}".to_vec(), 0)).await);
    assert!(reason.contains("payload"), "{reason}");
}

#[tokio::test]
async fn chat_cleanup_deletes_files_then_vector_store_and_row() {
    let fx = fx(5).await;
    let handler = ChatCleanupHandler::new(fx.deps.clone());
    let doc = fx.attachment("document", Some("file-doc"), false).await;
    let img = fx.attachment("image", Some("file-img"), false).await;
    let never = fx.attachment("document", None, false).await;
    // Deleted earlier through the attachment path; ownership moves to the
    // chat cleanup with the chat delete.
    let earlier = fx.attachment("document", Some("file-old"), true).await;
    fx.insert_vector_store("vs-1").await;
    fx.delete_chat().await;

    let res = handler.handle(&json_msg(&fx.chat_payload(), 0)).await;
    assert!(is_ok(&res), "{res:?}");
    let calls = fx.store.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    let mut files: Vec<&str> = calls[..3].iter().map(String::as_str).collect();
    files.sort_unstable();
    assert_eq!(
        files,
        vec![
            "delete_file:file-doc",
            "delete_file:file-img",
            "delete_file:file-old"
        ]
    );
    assert_eq!(calls[3], "delete_vs:vs-1", "vector store last");
    assert!(
        fx.store.aliases().iter().all(|a| a == TENANT_ALIAS),
        "files and the vector store go to the tenant override alias: {:?}",
        fx.store.aliases()
    );
    for a in [&doc, &img, &never, &earlier] {
        assert_eq!(fx.row(a.id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(fx.vector_stores().await.is_empty());

    // Redelivery: nothing left to do.
    assert!(is_ok(
        &handler.handle(&json_msg(&fx.chat_payload(), 1)).await
    ));
    assert_eq!(fx.store.calls().len(), 4);
}

#[tokio::test]
async fn chat_cleanup_keeps_vs_row_on_failure_and_retries() {
    let fx = fx(5).await;
    let handler = ChatCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), false).await;
    fx.insert_vector_store("vs-1").await;
    fx.delete_chat().await;
    let payload = fx.chat_payload();

    // A failing file delete keeps the attachment pending: no store delete yet.
    fx.store
        .script_files(vec![Err(StorageError::Transient("HTTP 502".to_owned()))]);
    assert!(is_retry(&handler.handle(&json_msg(&payload, 0)).await));
    assert_eq!(fx.store.calls(), vec!["delete_file:file-a"]);
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert_eq!(fx.vector_stores().await.len(), 1);

    // File done, store delete fails: Retry, row kept.
    fx.store.script_vs(vec![
        Err(StorageError::Transient("HTTP 500".to_owned())),
        Err(StorageError::Failed("HTTP 400".to_owned())),
    ]);
    assert!(is_retry(&handler.handle(&json_msg(&payload, 1)).await));
    assert_eq!(fx.row(a.id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(fx.vector_stores().await.len(), 1);

    // The delivery that reaches max_attempts (5) rejects; the row stays for a
    // dead-letter replay.
    let reason = reject_reason(handler.handle(&json_msg(&payload, 4)).await);
    assert_eq!(reason, "vector store delete: max attempts (5) reached");
    assert_eq!(fx.vector_stores().await.len(), 1);
    assert_eq!(
        fx.store.calls(),
        vec![
            "delete_file:file-a",
            "delete_file:file-a",
            "delete_vs:vs-1",
            "delete_vs:vs-1"
        ]
    );

    // Replay: the store is already gone (404) -> success, row deleted.
    fx.store.script_vs(vec![Err(StorageError::NotFound)]);
    assert!(is_ok(&handler.handle(&json_msg(&payload, 0)).await));
    assert!(fx.vector_stores().await.is_empty());
}

// ── Infrastructure failures, placeholders, failed attachments ────────────────

/// File-backed WAL database with a 50 ms busy timeout: a held write lock
/// turns the handler's writes into fast `SQLITE_BUSY` errors.
async fn busy_db() -> (tempfile::TempDir, Arc<DBProvider<DomainError>>) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!(
        "sqlite://{}?mode=rwc&journal_mode=wal&busy_timeout=50",
        dir.path().join("busy.db").display()
    );
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(5),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        crate::infra::db::all_migrations(),
    )
    .await
    .unwrap();
    (dir, Arc::new(DBProvider::new(db)))
}

#[tokio::test]
async fn attachment_cleanup_db_error_retries_without_counting() {
    let (_dir, db) = busy_db().await;
    let fx = fx_on(db, 5).await;
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    let a = fx.attachment("document", Some("file-a"), true).await;
    fx.store
        .script_files(vec![Err(StorageError::Transient("HTTP 503".to_owned()))]);
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let holder = Arc::new(Mutex::new(None));
    *fx.store.write_lock.lock().unwrap() = Some(WriteLock {
        db: Arc::clone(&fx.db),
        chat: fx.chat.clone(),
        release: release_rx,
        holder: Arc::clone(&holder),
    });

    // The provider delete fails, then recording the attempt hits the lock.
    let res = handler
        .handle(&json_msg(&Fx::attachment_payload(&a), 0))
        .await;
    release.send(()).unwrap();
    let handle = holder.lock().unwrap().take().unwrap();
    handle.await.unwrap();
    assert!(is_retry(&res), "{res:?}");
    assert_eq!(fx.store.calls(), vec!["delete_file:file-a"]);
    let row = fx.row(a.id).await;
    assert_eq!(
        row.cleanup_attempts, 0,
        "a DB failure is not a counted attempt"
    );
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.last_cleanup_error, None);
}

#[tokio::test]
async fn cleanup_identity_not_ready_retries_without_counting() {
    let fx = fx(1).await;
    let a = fx.attachment("document", Some("file-a"), true).await;
    fx.store.script_files(vec![Err(StorageError::Unavailable(
        "service identity not ready".to_owned(),
    ))]);
    let handler = AttachmentCleanupHandler::new(fx.deps.clone());
    // max_attempts = 1: a counted attempt would reject and mark `failed`.
    assert!(is_retry(
        &handler
            .handle(&json_msg(&Fx::attachment_payload(&a), 0))
            .await
    ));
    let row = fx.row(a.id).await;
    assert_eq!(row.cleanup_attempts, 0);
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));

    // Chat cleanup: same for files, and a store delete that never reached
    // the provider retries even on the last delivery.
    let chat_handler = ChatCleanupHandler::new(fx.deps.clone());
    fx.insert_vector_store("vs-1").await;
    fx.delete_chat().await;
    fx.store.script_files(vec![Err(StorageError::Unavailable(
        "service identity not ready".to_owned(),
    ))]);
    assert!(is_retry(
        &chat_handler.handle(&json_msg(&fx.chat_payload(), 0)).await
    ));
    assert_eq!(fx.row(a.id).await.cleanup_attempts, 0);
    fx.store.script_vs(vec![Err(StorageError::Unavailable(
        "service identity not ready".to_owned(),
    ))]);
    assert!(is_retry(
        &chat_handler.handle(&json_msg(&fx.chat_payload(), 5)).await
    ));
    assert_eq!(fx.row(a.id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(fx.vector_stores().await.len(), 1);
}

#[tokio::test]
async fn chat_cleanup_deletes_vector_store_despite_failed_attachment() {
    let fx = fx(1).await;
    let handler = ChatCleanupHandler::new(fx.deps.clone());
    let bad = fx.attachment("document", Some("file-bad"), false).await;
    let good = fx.attachment("document", Some("file-good"), false).await;
    fx.insert_vector_store("vs-1").await;
    fx.delete_chat().await;
    fx.store
        .script_files(vec![Err(StorageError::Failed("HTTP 400".to_owned()))]);

    let res = handler.handle(&json_msg(&fx.chat_payload(), 0)).await;
    assert!(is_ok(&res), "{res:?}");
    let statuses = [
        fx.row(bad.id).await.cleanup_status,
        fx.row(good.id).await.cleanup_status,
    ];
    let mut statuses: Vec<_> = statuses.iter().map(|s| s.as_deref().unwrap()).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, vec!["done", "failed"]);
    assert_eq!(fx.store.calls().last().unwrap(), "delete_vs:vs-1");
    assert!(fx.vector_stores().await.is_empty());
}

#[tokio::test]
async fn chat_cleanup_placeholder_deleted_or_retried_when_replaced() {
    // Placeholder only: deleted without a provider call.
    let fx = fx(5).await;
    let handler = ChatCleanupHandler::new(fx.deps.clone());
    fx.insert_placeholder().await;
    fx.delete_chat().await;
    assert!(is_ok(
        &handler.handle(&json_msg(&fx.chat_payload(), 0)).await
    ));
    assert!(fx.vector_stores().await.is_empty());
    assert!(fx.store.calls().is_empty());

    // The creator's CAS wins between the read and the delete: the handler
    // saw a placeholder, the row now has a store -> Retry, row kept; the
    // redelivery deletes the real store.
    fx.insert_vector_store("vs-real").await;
    let mut stale = fx.vector_stores().await.remove(0);
    stale.vector_store_id = None;
    let payload = fx.chat_payload();
    let res = handler
        .delete_vector_store(&payload, &stale, 0)
        .await
        .unwrap();
    assert!(is_retry(&res), "{res:?}");
    assert_eq!(fx.vector_stores().await.len(), 1);
    assert!(fx.store.calls().is_empty());

    assert!(is_ok(&handler.handle(&json_msg(&payload, 1)).await));
    assert_eq!(fx.store.calls(), vec!["delete_vs:vs-real"]);
    assert!(fx.vector_stores().await.is_empty());
}
