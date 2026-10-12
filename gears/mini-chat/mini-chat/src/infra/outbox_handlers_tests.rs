#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Unit tests of the usage, audit and attachment cleanup outbox handlers
//! (DESIGN section 5.6, section 3.2 "Audit plugin and audit outbox", section
//! 4 "Attachment Deletion", "Attachment cleanup state machine").

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatModelPolicyPluginError, PolicySnapshot, TurnMutationAuditEvent, UserLimits,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::*;
use crate::infra::audit_gateway::{AUDIT_PLUGIN_TIMEOUT, AuditPluginLocator};
use crate::infra::policy_gateway::{PluginLocator, PluginLookupError};

const INSTANCE_ID: &str = "instance-1";

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

fn is_reject(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Reject(_))
}

// ── Usage ────────────────────────────────────────────────────────────────

/// Policy plugin whose `publish_usage` returns a scripted result.
struct PublishingPolicy {
    result: Mutex<Result<(), PublishError>>,
    published: Mutex<Vec<UsageEvent>>,
}

impl PublishingPolicy {
    fn new(result: Result<(), PublishError>) -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(result),
            published: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for PublishingPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Internal(
            "unused".to_owned(),
        ))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Internal(
            "unused".to_owned(),
        ))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Internal(
            "unused".to_owned(),
        ))
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().unwrap().push(payload);
        self.result.lock().unwrap().clone()
    }
}

fn usage_event() -> UsageEvent {
    let tenant_id = Uuid::from_u128(1);
    let turn_id = Uuid::from_u128(2);
    let request_id = Uuid::from_u128(3);
    UsageEvent {
        tenant_id,
        user_id: Some(Uuid::from_u128(7)),
        chat_id: Some(Uuid::from_u128(9)),
        turn_id: Some(turn_id),
        request_id,
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 1,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "user".to_owned(),
        dedupe_key: mini_chat_sdk::dedupe_key_for_turn(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

fn usage_message() -> OutboxMessage {
    message(serde_json::to_vec(&usage_event()).unwrap(), 0)
}

#[tokio::test]
async fn usage_is_published_and_acknowledged() {
    let policy = PublishingPolicy::new(Ok(()));
    let handler = UsageHandler::new(policy.clone());

    let result = LeasedMessageHandler::handle(&handler, &usage_message()).await;

    assert!(is_ok(&result), "{result:?}");
    let published = policy.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].dedupe_key, usage_event().dedupe_key);
}

#[tokio::test]
async fn transient_publish_failure_is_retried() {
    let policy = PublishingPolicy::new(Err(PublishError::Transient("down".to_owned())));
    let handler = UsageHandler::new(policy);

    let result = LeasedMessageHandler::handle(&handler, &usage_message()).await;

    assert!(is_retry(&result), "{result:?}");
}

#[tokio::test]
async fn permanent_publish_failure_is_rejected() {
    let policy = PublishingPolicy::new(Err(PublishError::Permanent("invalid".to_owned())));
    let handler = UsageHandler::new(policy);

    let result = LeasedMessageHandler::handle(&handler, &usage_message()).await;

    assert!(is_reject(&result), "{result:?}");
}

#[tokio::test]
async fn malformed_usage_payload_is_rejected_without_publishing() {
    let policy = PublishingPolicy::new(Ok(()));
    let handler = UsageHandler::new(policy.clone());

    let result = LeasedMessageHandler::handle(&handler, &message(b"{\"x\":1}".to_vec(), 0)).await;

    assert!(is_reject(&result), "{result:?}");
    assert!(policy.published.lock().unwrap().is_empty());
}

// ── Audit ────────────────────────────────────────────────────────────────

/// Locator with a fixed lookup result and client; counts lookups.
struct FakeLocator {
    lookup: Result<String, PluginLookupError>,
    client: Option<Arc<dyn MiniChatAuditPluginClientV1>>,
    lookups: AtomicUsize,
}

#[async_trait]
impl PluginLocator<dyn MiniChatAuditPluginClientV1> for FakeLocator {
    async fn find_instance(&self) -> Result<String, PluginLookupError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.lookup.clone()
    }

    fn client(&self, _instance_id: &str) -> Option<Arc<dyn MiniChatAuditPluginClientV1>> {
        self.client.clone()
    }
}

/// Audit plugin with a scripted `emit` result.
struct ScriptedAudit(Result<(), MiniChatAuditPluginError>);

#[async_trait]
impl MiniChatAuditPluginClientV1 for ScriptedAudit {
    async fn emit(&self, _event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.0.clone()
    }
}

fn audit_handler(
    lookup: Result<String, PluginLookupError>,
    emit: Option<Result<(), MiniChatAuditPluginError>>,
) -> (AuditHandler, Arc<FakeLocator>) {
    let locator = Arc::new(FakeLocator {
        lookup,
        client: emit.map(|result| Arc::new(ScriptedAudit(result)) as _),
        lookups: AtomicUsize::new(0),
    });
    let gateway = AuditGateway::with_locator(
        Arc::clone(&locator) as Arc<AuditPluginLocator>,
        AUDIT_PLUGIN_TIMEOUT,
    );
    (AuditHandler::new(Arc::new(gateway), "mini_chat"), locator)
}

fn audit_message(attempts: i16) -> OutboxMessage {
    let event: AuditEvent = TurnMutationAuditEvent::delete(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        Uuid::from_u128(4),
        OffsetDateTime::UNIX_EPOCH,
    )
    .into();
    message(serde_json::to_vec(&event).unwrap(), attempts)
}

fn not_registered() -> PluginLookupError {
    PluginLookupError::NotRegistered {
        plugin: "audit",
        vendor: "acme".to_owned(),
    }
}

#[tokio::test]
async fn delivered_audit_event_is_acknowledged() {
    let (handler, _) = audit_handler(Ok(INSTANCE_ID.to_owned()), Some(Ok(())));

    let result = LeasedMessageHandler::handle(&handler, &audit_message(0)).await;

    assert!(is_ok(&result), "{result:?}");
}

#[tokio::test]
async fn audit_event_without_plugin_is_dropped_and_acknowledged() {
    let (handler, _) = audit_handler(Err(not_registered()), None);

    let result = LeasedMessageHandler::handle(&handler, &audit_message(0)).await;

    assert!(is_ok(&result), "{result:?}");
}

#[tokio::test]
async fn malformed_audit_payload_is_rejected_before_plugin_lookup() {
    let (handler, locator) = audit_handler(Err(not_registered()), None);

    let result = LeasedMessageHandler::handle(&handler, &message(b"not json".to_vec(), 0)).await;

    assert!(is_reject(&result), "{result:?}");
    assert_eq!(locator.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transient_audit_failure_is_retried_until_the_last_attempt() {
    let transient = || Some(Err(MiniChatAuditPluginError::Transient("busy".to_owned())));

    let (handler, _) = audit_handler(Ok(INSTANCE_ID.to_owned()), transient());
    let result = LeasedMessageHandler::handle(&handler, &audit_message(0)).await;
    assert!(is_retry(&result), "{result:?}");

    // `attempts` counts retries from 0: 118 is the 119th attempt.
    let result = LeasedMessageHandler::handle(&handler, &audit_message(118)).await;
    assert!(is_retry(&result), "{result:?}");

    let result = LeasedMessageHandler::handle(&handler, &audit_message(119)).await;
    assert!(is_reject(&result), "{result:?}");
}

#[tokio::test]
async fn resolution_failure_is_retried() {
    let (handler, _) = audit_handler(
        Err(PluginLookupError::Unavailable("registry down".to_owned())),
        None,
    );

    let result = LeasedMessageHandler::handle(&handler, &audit_message(3)).await;

    assert!(is_retry(&result), "{result:?}");
}

#[tokio::test]
async fn permanent_audit_failure_is_rejected() {
    let (handler, _) = audit_handler(
        Ok(INSTANCE_ID.to_owned()),
        Some(Err(MiniChatAuditPluginError::Permanent("bad".to_owned()))),
    );

    let result = LeasedMessageHandler::handle(&handler, &audit_message(0)).await;

    assert!(is_reject(&result), "{result:?}");
}

#[test]
fn audit_outcomes_follow_the_design_table() {
    let cases = [
        (AuditDelivery::Delivered, 1, AuditEmitResult::Ok),
        (AuditDelivery::Dropped, 1, AuditEmitResult::Dropped),
        (
            AuditDelivery::Retry("x".to_owned()),
            1,
            AuditEmitResult::Retry,
        ),
        (
            AuditDelivery::Retry("x".to_owned()),
            AUDIT_MAX_ATTEMPTS,
            AuditEmitResult::Reject,
        ),
        (
            AuditDelivery::Reject("x".to_owned()),
            1,
            AuditEmitResult::Reject,
        ),
    ];
    for (delivery, attempt, expected) in cases {
        let (_, emitted) = audit_outcome(delivery, attempt);
        assert_eq!(emitted, expected);
    }
    assert_eq!(AuditEmitResult::Dropped.as_str(), "dropped");
}

// ── Attachment cleanup ───────────────────────────────────────────────────

mod attachment_cleanup {
    use toolkit_db::secure::SecureUpdateExt;
    use toolkit_security::AccessScope;

    use super::*;
    use crate::domain::attachment_service::tests::{Call, FakeStorage};
    use crate::domain::ports::{
        AttachmentKind, ChatUploadLimits, NewAttachment, NewChat, StorageError,
    };
    use crate::infra::db::entities::attachments;
    use crate::infra::db::repo::tests::{
        TENANT_A, TestStore, USER_A1, at, attachment_row, subject_scope,
    };
    use crate::infra::db::repo::{attachments as attachment_repo, chats as chat_repo};
    use crate::infra::outbox::AttachmentCleanupEvent;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    struct Fixture {
        store: TestStore,
        storage: Arc<FakeStorage>,
        chat_id: Uuid,
    }

    impl Fixture {
        async fn new() -> Self {
            let store = TestStore::without_outbox().await;
            let chat_id = Uuid::new_v4();
            chat_repo::insert(
                &store.conn(),
                store.ts(),
                &AccessScope::allow_all(),
                NewChat {
                    id: chat_id,
                    tenant_id: TENANT_A,
                    user_id: USER_A1,
                    model: "standard".to_owned(),
                    title: None,
                    now: at(0),
                },
            )
            .await
            .unwrap();
            Self {
                store,
                storage: FakeStorage::new(),
                chat_id,
            }
        }

        fn handler(&self, max_attempts: u32) -> AttachmentCleanupHandler {
            AttachmentCleanupHandler::new(
                Arc::clone(&self.store.repo),
                Arc::clone(&self.storage) as Arc<dyn FileStorage>,
                max_attempts,
            )
        }

        /// An attachment with `provider_file_id`, deleted with `cleanup_status = pending`.
        async fn deleted_attachment(&self, provider_file_id: Option<&str>) -> Uuid {
            let id = Uuid::new_v4();
            let scope = subject_scope(TENANT_A, USER_A1);
            attachment_repo::insert_pending(
                &self.store.conn(),
                self.store.ts(),
                &scope,
                NewAttachment {
                    id,
                    tenant_id: TENANT_A,
                    chat_id: self.chat_id,
                    uploaded_by_user_id: USER_A1,
                    filename: "a.pdf".to_owned(),
                    content_type: "application/pdf".to_owned(),
                    size_bytes: 1,
                    storage_backend: "openai".to_owned(),
                    kind: AttachmentKind::Document,
                    for_file_search: true,
                    for_code_interpreter: false,
                    now: at(0),
                },
                ChatUploadLimits {
                    max_documents: 100,
                    max_total_bytes: u64::MAX,
                },
            )
            .await
            .unwrap();
            if let Some(file_id) = provider_file_id {
                attachment_repo::mark_uploaded(
                    &self.store.conn(),
                    self.store.ts(),
                    &scope,
                    (self.chat_id, id),
                    file_id,
                    at(0),
                )
                .await
                .unwrap();
            }
            self.set_cleanup_status(id, Some("pending")).await;
            id
        }

        async fn set_cleanup_status(&self, id: Uuid, status: Option<&str>) {
            attachments::Entity::update_many()
                .col_expr(
                    attachments::Column::CleanupStatus,
                    Expr::value(status.map(str::to_owned)),
                )
                .col_expr(
                    attachments::Column::DeletedAt,
                    Expr::value(Some(self.store.stamp(at(0)))),
                )
                .filter(attachments::Column::Id.eq(id))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .exec(&self.store.conn())
                .await
                .unwrap();
        }

        fn payload(
            &self,
            attachment_id: Uuid,
            provider_file_id: Option<&str>,
        ) -> AttachmentCleanupPayload {
            AttachmentCleanupPayload {
                event_type: AttachmentCleanupEvent::Deleted,
                tenant_id: TENANT_A,
                chat_id: self.chat_id,
                attachment_id,
                provider_file_id: provider_file_id.map(str::to_owned),
                vector_store_id: None,
                storage_backend: "openai".to_owned(),
                attachment_kind: "document".to_owned(),
                deleted_at: at(0),
                secondary_ref: None,
            }
        }

        fn deletes(&self) -> Vec<String> {
            self.storage
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::DeleteFile(id) => Some(id),
                    _ => None,
                })
                .collect()
        }

        fn fail_deletes(&self, times: usize) {
            let mut deletes = self.storage.deletes.lock().unwrap();
            for _ in 0..times {
                deletes.push_back(Err(StorageError::Transient("HTTP 500: boom".to_owned())));
            }
        }
    }

    async fn handle(
        handler: &AttachmentCleanupHandler,
        payload: &AttachmentCleanupPayload,
        attempts: i16,
    ) -> MessageResult {
        handler
            .handle(&message(serde_json::to_vec(payload).unwrap(), attempts))
            .await
    }

    #[tokio::test]
    async fn malformed_payload_is_rejected() {
        let fx = Fixture::new().await;

        let result = fx
            .handler(5)
            .handle(&message(
                br#"{"event_type":"attachment_deleted"}"#.to_vec(),
                0,
            ))
            .await;

        assert!(is_reject(&result), "{result:?}");
        assert!(fx.storage.calls().is_empty());
    }

    #[tokio::test]
    async fn successful_delete_marks_cleanup_done() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;

        let result = handle(&fx.handler(5), &fx.payload(id, Some("file-1")), 0).await;

        assert!(is_ok(&result), "{result:?}");
        assert_eq!(fx.deletes(), ["file-1"]);
        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"));
        assert!(row.cleanup_updated_at.is_some());
        assert_eq!(row.cleanup_attempts, 0);
    }

    #[tokio::test]
    async fn attachment_without_provider_file_is_done_without_a_call() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(None).await;

        let result = handle(&fx.handler(5), &fx.payload(id, None), 0).await;

        assert!(is_ok(&result));
        assert!(fx.storage.calls().is_empty());
        assert_eq!(
            attachment_row(&fx.store, id)
                .await
                .cleanup_status
                .as_deref(),
            Some("done")
        );
    }

    #[tokio::test]
    async fn provider_file_recorded_after_the_delete_is_deleted_too() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-late")).await;

        let result = handle(&fx.handler(5), &fx.payload(id, None), 0).await;

        assert!(is_ok(&result));
        assert_eq!(fx.deletes(), ["file-late"]);
    }

    #[tokio::test]
    async fn failed_delete_is_retried_then_rejected_at_max_attempts() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        fx.fail_deletes(3);
        let handler = fx.handler(2);
        let payload = fx.payload(id, Some("file-1"));

        let first = handle(&handler, &payload, 0).await;
        assert!(is_retry(&first), "{first:?}");
        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_attempts, 1);
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        assert!(
            row.last_cleanup_error
                .as_deref()
                .unwrap()
                .contains("HTTP 500")
        );
        assert!(row.cleanup_updated_at.is_some());

        let second = handle(&handler, &payload, 1).await;
        assert!(is_reject(&second), "{second:?}");
        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_attempts, 2);
        assert_eq!(row.cleanup_status.as_deref(), Some("failed"));

        // A redelivery (dead-letter replay) of a terminal row does nothing.
        let replay = handle(&handler, &payload, 0).await;
        assert!(is_ok(&replay));
        assert_eq!(fx.deletes().len(), 2);
    }

    #[tokio::test]
    async fn retried_delete_succeeds_after_failures() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        fx.fail_deletes(2);
        let handler = fx.handler(5);
        let payload = fx.payload(id, Some("file-1"));

        assert!(is_retry(&handle(&handler, &payload, 0).await));
        assert!(is_retry(&handle(&handler, &payload, 1).await));
        assert!(is_ok(&handle(&handler, &payload, 2).await));

        let row = attachment_row(&fx.store, id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"));
        assert_eq!(row.cleanup_attempts, 2);
        assert_eq!(fx.deletes().len(), 3);
    }

    #[tokio::test]
    async fn soft_deleted_chat_is_acknowledged_without_a_call() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        chat_repo::mark_deleted(
            &fx.store.conn(),
            fx.store.ts(),
            &AccessScope::allow_all(),
            fx.chat_id,
            at(0),
        )
        .await
        .unwrap();

        let result = handle(&fx.handler(5), &fx.payload(id, Some("file-1")), 0).await;

        assert!(is_ok(&result));
        assert!(fx.storage.calls().is_empty());
        assert_eq!(
            attachment_row(&fx.store, id)
                .await
                .cleanup_status
                .as_deref(),
            Some("pending"),
            "chat cleanup owns the row"
        );
    }

    #[tokio::test]
    async fn unknown_attachment_or_other_tenant_is_acknowledged() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        let mut foreign = fx.payload(id, Some("file-1"));
        foreign.tenant_id = Uuid::from_u128(0xdead);

        assert!(is_ok(
            &handle(
                &fx.handler(5),
                &fx.payload(Uuid::new_v4(), Some("file-x")),
                0
            )
            .await
        ));
        assert!(is_ok(&handle(&fx.handler(5), &foreign, 0).await));
        assert!(fx.storage.calls().is_empty());
    }

    #[tokio::test]
    async fn already_done_cleanup_is_not_repeated() {
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        fx.set_cleanup_status(id, Some("done")).await;

        assert!(is_ok(
            &handle(&fx.handler(5), &fx.payload(id, Some("file-1")), 0).await
        ));
        assert!(fx.storage.calls().is_empty());
    }

    /// Records secondary deletes; fails every call when `fail` is set.
    struct RecordingSecondary {
        deleted: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl SecondaryFileStorage for RecordingSecondary {
        async fn delete_secondary_file(
            &self,
            _tenant_id: Uuid,
            file_id: &str,
            _provider_kind: &str,
            _upstream_alias: &str,
        ) -> Result<(), StorageError> {
            self.deleted.lock().unwrap().push(file_id.to_owned());
            if self.fail {
                Err(StorageError::Transient("down".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn secondary_copy_is_deleted_best_effort() {
        for fail in [false, true] {
            let fx = Fixture::new().await;
            let id = fx.deleted_attachment(Some("file-1")).await;
            let secondary = Arc::new(RecordingSecondary {
                deleted: Mutex::new(Vec::new()),
                fail,
            });
            let handler = fx
                .handler(5)
                .with_secondary(Arc::clone(&secondary) as Arc<dyn SecondaryFileStorage>);
            let mut payload = fx.payload(id, Some("file-1"));
            payload.secondary_ref = Some(SecondaryFileRef {
                file_id: "anthropic-file".to_owned(),
                provider_kind: "anthropic".to_owned(),
                upstream_alias: "api.anthropic.com".to_owned(),
            });

            assert!(is_ok(&handle(&handler, &payload, 0).await));
            assert_eq!(*secondary.deleted.lock().unwrap(), ["anthropic-file"]);
            assert_eq!(
                attachment_row(&fx.store, id)
                    .await
                    .cleanup_status
                    .as_deref(),
                Some("done")
            );
        }

        // Without a secondary storage the primary cleanup still completes.
        let fx = Fixture::new().await;
        let id = fx.deleted_attachment(Some("file-1")).await;
        let mut payload = fx.payload(id, Some("file-1"));
        payload.secondary_ref = Some(SecondaryFileRef {
            file_id: "anthropic-file".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: "api.anthropic.com".to_owned(),
        });
        assert!(is_ok(&handle(&fx.handler(5), &payload, 0).await));
    }

    #[tokio::test]
    async fn handler_is_installed_on_the_attachment_cleanup_queue() {
        let fx = Fixture::new().await;
        let queues = crate::infra::outbox::OutboxQueues::default();
        let handler = Arc::new(fx.handler(5));
        let handlers = crate::infra::outbox_pipeline::QueueHandlers::deferred(&queues)
            .with_attachment_cleanup(Arc::clone(&handler));

        let installed = handlers.get(crate::infra::outbox::MiniChatQueue::AttachmentCleanup);
        assert_eq!(
            Arc::as_ptr(&installed).cast::<()>(),
            Arc::as_ptr(&handler).cast::<()>()
        );
        let usage = handlers.get(crate::infra::outbox::MiniChatQueue::Usage);
        assert_ne!(
            Arc::as_ptr(&usage).cast::<()>(),
            Arc::as_ptr(&handler).cast::<()>()
        );
    }
}
