//! Attachment cleanup handler and the provider-file cleanup shared with the chat cleanup
//! (DESIGN "Attachment Deletion" phase 2 and "Cleanup on Chat Deletion").
//!
//! The attachment's `cleanup_status` / `cleanup_attempts` carry the retry budget
//! (`cleanup_worker.max_attempts`); the outbox only paces the redeliveries.

use std::sync::Arc;

use async_trait::async_trait;
use opentelemetry::KeyValue;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use super::payloads::AttachmentCleanupEvent;
use crate::api::state::AppServices;
use crate::domain::error::DomainError;
use crate::infra::db::CleanupStatus;
use crate::infra::db::repo;
use crate::infra::db::repo::attachments::CleanupFailure;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx;
use crate::infra::llm::{ProviderResolver, S2sContext};
use crate::infra::storage::{AnthropicFiles, FileStorage, VectorStores};
use crate::metrics::Metrics;

/// Longest `last_cleanup_error` stored, in characters.
const ERROR_MAX_CHARS: usize = 500;

/// What the cleanup handlers need. Background work: queries run with `AccessScope::allow_all()`
/// and always filter by the payload's `tenant_id` and chat / attachment ids.
#[derive(Clone)]
pub struct CleanupDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub files: Arc<dyn FileStorage>,
    pub vector_stores: Arc<dyn VectorStores>,
    pub resolver: Arc<ProviderResolver>,
    pub s2s: S2sContext,
    /// Anthropic Files client for secondary copies (`None` without an `anthropic_messages`
    /// provider: secondary deletions are skipped and counted).
    pub anthropic_files: Option<Arc<AnthropicFiles>>,
    /// `cleanup_worker.max_attempts`.
    pub max_attempts: u32,
    pub metrics: Arc<Metrics>,
}

/// The secondary (Anthropic) copy of an attachment to delete with its primary file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SecondaryFile {
    pub file_id: String,
    pub provider_kind: String,
    /// Upstream alias of the Files API; `None` when it could not be resolved.
    pub alias: Option<String>,
}

/// The provider file(s) of one attachment and the cleanup state they were read with.
pub(super) struct AttachmentFiles<'a> {
    pub tenant_id: Uuid,
    pub id: Uuid,
    /// Upstream label of the primary file.
    pub backend: &'a str,
    /// `None`: the upload never reached the provider.
    pub file_id: Option<&'a str>,
    /// `cleanup_attempts` of the row when it was read (compare-and-set of a failed attempt).
    pub attempts: i32,
    pub secondary: Option<SecondaryFile>,
}

/// Counts one cleanup event of `resource_type` (`file` / `vector_store`) with an optional `reason`.
pub(super) fn count(
    counter: &opentelemetry::metrics::Counter<u64>,
    resource_type: &'static str,
    reason: Option<&'static str>,
) {
    let mut labels = vec![KeyValue::new("resource_type", resource_type)];
    if let Some(reason) = reason {
        labels.push(KeyValue::new("reason", reason));
    }
    counter.add(1, &labels);
}

/// Result of cleaning one attachment's provider file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileOutcome {
    /// `cleanup_status = done` (also when another delivery got there first).
    Done,
    /// The delete failed; `cleanup_status` stays `pending` for a later attempt.
    Pending,
    /// The attempts reached the limit; `cleanup_status = failed`.
    Failed,
}

impl CleanupDeps {
    #[must_use]
    pub fn from_services(services: &AppServices) -> Self {
        Self {
            db: Arc::clone(&services.db),
            files: Arc::clone(&services.files),
            vector_stores: Arc::clone(&services.vector_stores),
            resolver: Arc::clone(&services.providers),
            s2s: services.s2s.clone(),
            anthropic_files: services.anthropic_files.clone(),
            max_attempts: services.cfg.cleanup_worker.max_attempts,
            metrics: Arc::clone(&services.metrics),
        }
    }

    /// Fails with `ProviderUnavailable` while the S2S context is not set (start-up): the
    /// delivery is retried without costing an attempt.
    pub(super) fn require_s2s(&self) -> Result<(), DomainError> {
        self.s2s.get().map(drop)
    }

    /// Deletes provider file `file_id` through the upstream of `backend`; the error text is the
    /// failed attempt's reason. A missing file counts as deleted.
    async fn delete_provider_file(
        &self,
        tenant_id: Uuid,
        backend: &str,
        file_id: &str,
    ) -> Result<(), String> {
        let target = self
            .resolver
            .storage_by_backend(backend, tenant_id)
            .map_err(|err| err.to_string())?;
        self.files
            .delete(&target, file_id)
            .await
            .map_err(|err| err.to_string())
    }

    /// Deletes vector store `vector_store_id` through the upstream of `backend`; the error text is
    /// the failed attempt's reason. A missing store counts as deleted.
    pub(super) async fn delete_vector_store(
        &self,
        tenant_id: Uuid,
        backend: &str,
        vector_store_id: &str,
    ) -> Result<(), String> {
        let target = self
            .resolver
            .storage_by_backend(backend, tenant_id)
            .map_err(|err| err.to_string())?;
        self.vector_stores
            .delete(&target, vector_store_id)
            .await
            .map_err(|err| err.to_string())
    }

    /// Cleans the provider file(s) of one attachment and records the outcome on its row.
    ///
    /// A missing `file_id` means the upload never reached the provider (done at once). After the
    /// primary delete succeeded the secondary copy (if any) is deleted best effort.
    ///
    /// # Errors
    /// A database error or a missing S2S context: nothing was counted, retry the delivery.
    pub(super) async fn clean_attachment(
        &self,
        files: AttachmentFiles<'_>,
    ) -> Result<FileOutcome, DomainError> {
        let result = match files.file_id {
            None => Ok(()),
            Some(file_id) => {
                self.require_s2s()?;
                self.delete_provider_file(files.tenant_id, files.backend, file_id)
                    .await
            }
        };
        if result.is_ok()
            && let Some(secondary) = &files.secondary
        {
            self.delete_secondary_file(files.id, secondary).await;
        }
        self.record_outcome(files.tenant_id, files.id, files.attempts, result)
            .await
    }

    /// Best-effort deletion of the secondary (Anthropic) copy of a file: a failure is logged
    /// and never blocks the cleanup; without a files client or an alias it is skipped and
    /// counted.
    async fn delete_secondary_file(&self, id: Uuid, secondary: &SecondaryFile) {
        let (Some(files), Some(alias)) = (&self.anthropic_files, &secondary.alias) else {
            tracing::warn!(attachment_id = %id, provider_kind = %secondary.provider_kind,
                "secondary file deletion skipped: no files client or upstream alias");
            self.metrics.secondary_cleanup_skipped.add(
                1,
                &[KeyValue::new(
                    "provider_kind",
                    secondary.provider_kind.clone(),
                )],
            );
            return;
        };
        if let Err(err) = files.delete(alias, &secondary.file_id).await {
            tracing::warn!(attachment_id = %id, error = %err, "secondary file delete failed");
        }
    }

    async fn record_outcome(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        observed_attempts: i32,
        result: Result<(), String>,
    ) -> Result<FileOutcome, DomainError> {
        let scope = AccessScope::allow_all();
        match result {
            Ok(()) => {
                let marked = write_tx(&self.db, |tx| {
                    let scope = scope.clone();
                    Box::pin(async move {
                        repo::attachments::mark_cleanup_done(tx, &scope, tenant_id, id, db_now())
                            .await
                    })
                })
                .await?;
                if marked {
                    count(&self.metrics.cleanup_completed, "file", None);
                }
                Ok(FileOutcome::Done)
            }
            Err(reason) => {
                tracing::warn!(attachment_id = %id, %reason, "provider file delete failed");
                let reason: String = reason.chars().take(ERROR_MAX_CHARS).collect();
                let max_attempts = self.max_attempts;
                let recorded = write_tx(&self.db, |tx| {
                    let (scope, reason) = (scope.clone(), reason.clone());
                    Box::pin(async move {
                        repo::attachments::record_cleanup_failure(
                            tx,
                            &scope,
                            tenant_id,
                            id,
                            &reason,
                            max_attempts,
                            observed_attempts,
                            db_now(),
                        )
                        .await
                    })
                })
                .await?;
                Ok(self.failure_outcome(id, recorded))
            }
        }
    }

    /// The outcome of a failed delete. Metrics are counted only for the transition this
    /// delivery wrote; a lost compare-and-set follows the row as another delivery left it.
    fn failure_outcome(&self, id: Uuid, recorded: CleanupFailure) -> FileOutcome {
        match recorded {
            CleanupFailure::Recorded(CleanupStatus::Failed) => {
                count(&self.metrics.cleanup_failed, "file", None);
                FileOutcome::Failed
            }
            CleanupFailure::Recorded(_) => {
                count(&self.metrics.cleanup_retry, "file", Some("provider_error"));
                FileOutcome::Pending
            }
            CleanupFailure::Lost(status) => {
                tracing::debug!(attachment_id = %id, ?status, "cleanup row changed by another delivery");
                match status {
                    Some(CleanupStatus::Pending) => FileOutcome::Pending,
                    Some(CleanupStatus::Failed) => FileOutcome::Failed,
                    Some(CleanupStatus::Done) | None => FileOutcome::Done,
                }
            }
        }
    }
}

/// Deletes the provider file of one attachment (`attachment_deleted`,
/// `attachment_upload_abandoned`, `attachment_indexing_failed`).
///
/// Acks without action when the parent chat is soft-deleted (the chat cleanup owns its files) or
/// when the attachment has no pending cleanup. A failed delete is `Retry` (attempt recorded on
/// the row) until `max_attempts`, then the row becomes `failed` and the message is `Reject`ed.
/// A database error is `Retry` without an attempt; a malformed payload is `Reject`ed.
pub struct AttachmentCleanupHandler {
    deps: CleanupDeps,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(deps: CleanupDeps) -> Self {
        Self { deps }
    }

    async fn process(&self, ev: &AttachmentCleanupEvent) -> Result<MessageResult, DomainError> {
        let deps = &self.deps;
        let scope = AccessScope::allow_all();
        let conn = deps.db.conn()?;
        let chat = repo::chats::find_any(&conn, &scope, ev.tenant_id, ev.chat_id).await?;
        if chat.is_some_and(|c| c.deleted_at.is_some()) {
            return Ok(MessageResult::Ok);
        }
        let row = repo::attachments::find_for_cleanup(
            &conn,
            &scope,
            ev.tenant_id,
            ev.chat_id,
            ev.attachment_id,
        )
        .await?;
        let pending = row
            .as_ref()
            .and_then(|r| r.cleanup_status.as_deref())
            .and_then(CleanupStatus::parse)
            == Some(CleanupStatus::Pending);
        if !pending {
            tracing::warn!(
                attachment_id = %ev.attachment_id,
                found = row.is_some(),
                "attachment cleanup without a pending row; nothing to do"
            );
            return Ok(MessageResult::Ok);
        }
        let attempts = row.as_ref().map_or(0, |r| r.cleanup_attempts);
        let secondary = ev.secondary_ref.as_ref().map(|s| SecondaryFile {
            file_id: s.file_id.clone(),
            provider_kind: s.provider_kind.clone(),
            alias: Some(s.upstream_alias.clone()),
        });
        let outcome = deps
            .clean_attachment(AttachmentFiles {
                tenant_id: ev.tenant_id,
                id: ev.attachment_id,
                backend: &ev.storage_backend,
                file_id: ev.provider_file_id.as_deref(),
                attempts,
                secondary,
            })
            .await?;
        Ok(match outcome {
            FileOutcome::Done => MessageResult::Ok,
            FileOutcome::Pending => MessageResult::Retry,
            FileOutcome::Failed => MessageResult::Reject(format!(
                "provider file delete: max attempts ({}) reached",
                deps.max_attempts
            )),
        })
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(err) => {
                return MessageResult::Reject(format!(
                    "malformed attachment cleanup payload: {err}"
                ));
            }
        };
        match self.process(&ev).await {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(attachment_id = %ev.attachment_id, error = %err, "attachment cleanup deferred");
                MessageResult::Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use serde_json::json;
    use time::OffsetDateTime;
    use toolkit_db::secure::AccessScope;
    use uuid::Uuid;

    use super::*;
    use crate::infra::db::entity::attachments;
    use crate::infra::db::repo;
    use crate::infra::db::ts::db_now;
    use crate::infra::outbox::{ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AttachmentCleanupEvent};
    use std::time::Duration;

    use crate::test_support::app::{ANTHROPIC_ALIAS, TestApp, anthropic_config, ctx};
    use crate::test_support::attachments::{attachment_row, set_cleanup_state};
    use crate::test_support::gateway::Responder;
    use crate::test_support::metrics::MetricsProbe;
    use crate::test_support::outbox::{outbox_message, raw_outbox_message};
    use crate::test_support::stream::{CHATS, SeedAttachment, create_chat, seed_attachment};

    /// A chat with one seeded document whose cleanup is pending (as after `DELETE attachment`).
    struct Fixture {
        app: TestApp,
        tenant: Uuid,
        user: Uuid,
        chat: Uuid,
        att: Uuid,
        file: String,
    }

    async fn fixture() -> Fixture {
        // The queue's own handler must not race the direct calls of the tests.
        fixture_in(TestApp::builder().quiet_cleanup().build().await).await
    }

    async fn fixture_in(app: TestApp) -> Fixture {
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;
        let att = seed_attachment(&app, SeedAttachment::document(tenant, chat, user)).await;
        let conn = app.db.conn().expect("conn");
        assert!(
            repo::attachments::soft_delete(&conn, &AccessScope::allow_all(), att, db_now())
                .await
                .unwrap()
        );
        let file = format!("file-{}", att.simple());
        Fixture {
            app,
            tenant,
            user,
            chat,
            att,
            file,
        }
    }

    impl Fixture {
        fn handler(&self) -> AttachmentCleanupHandler {
            AttachmentCleanupHandler::new(CleanupDeps::from_services(&self.app.services))
        }

        fn event(&self) -> AttachmentCleanupEvent {
            AttachmentCleanupEvent {
                event_type: "attachment_deleted".to_owned(),
                tenant_id: self.tenant,
                chat_id: self.chat,
                attachment_id: self.att,
                provider_file_id: Some(self.file.clone()),
                vector_store_id: None,
                storage_backend: "openai".to_owned(),
                attachment_kind: "document".to_owned(),
                deleted_at: OffsetDateTime::now_utc(),
                secondary_ref: None,
            }
        }

        fn message(&self, attempts: i16) -> OutboxMessage {
            outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &self.event(), attempts)
        }

        fn file_path(&self) -> String {
            format!("/v1/files/{}", self.file)
        }

        fn deletes(&self) -> usize {
            self.app
                .gateway
                .requests_to(&Method::DELETE, &self.file_path())
                .len()
        }

        async fn row(&self) -> attachments::Model {
            attachment_row(&self.app, self.chat, self.att).await
        }
    }

    #[tokio::test]
    async fn attachment_cleanup_deletes_file_and_marks_done() {
        let f = fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(200, json!({"deleted": true})),
        );
        assert!(matches!(
            f.handler().handle(&f.message(0)).await,
            MessageResult::Ok
        ));
        assert_eq!(f.deletes(), 1);
        let row = f.row().await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"));
        assert_eq!(row.cleanup_attempts, 0);
        assert!(row.cleanup_updated_at.is_some());
    }

    #[tokio::test]
    async fn attachment_cleanup_treats_404_as_done() {
        let f = fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(404, json!({"error": {"message": "No such file"}})),
        );
        assert!(matches!(
            f.handler().handle(&f.message(0)).await,
            MessageResult::Ok
        ));
        assert_eq!(f.row().await.cleanup_status.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn attachment_cleanup_failures_count_up_to_max_attempts_then_fail() {
        let f = fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(500, json!({"error": {"message": "boom file-secret"}})),
        );
        let handler = f.handler();
        for attempt in 1..=4_i16 {
            let res = handler.handle(&f.message(attempt - 1)).await;
            assert!(
                matches!(res, MessageResult::Retry),
                "attempt {attempt}: {res:?}"
            );
            let row = f.row().await;
            assert_eq!(row.cleanup_attempts, i32::from(attempt));
            assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
            assert!(row.last_cleanup_error.is_some());
        }
        let res = handler.handle(&f.message(4)).await;
        assert!(matches!(res, MessageResult::Reject(_)), "{res:?}");
        let row = f.row().await;
        assert_eq!(row.cleanup_attempts, 5);
        assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
        assert_eq!(f.deletes(), 5);

        // Terminal: a replay does nothing.
        assert!(matches!(
            handler.handle(&f.message(5)).await,
            MessageResult::Ok
        ));
        assert_eq!(f.deletes(), 5);
    }

    #[tokio::test]
    async fn attachment_cleanup_recovers_after_a_failed_attempt() {
        let f = fixture().await;
        f.app.gateway.on_sequence(
            Method::DELETE,
            &f.file_path(),
            vec![
                Responder::json(503, json!({})),
                Responder::json(200, json!({"deleted": true})),
            ],
        );
        let handler = f.handler();
        assert!(matches!(
            handler.handle(&f.message(0)).await,
            MessageResult::Retry
        ));
        assert!(matches!(
            handler.handle(&f.message(1)).await,
            MessageResult::Ok
        ));
        let row = f.row().await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"));
        assert_eq!(row.cleanup_attempts, 1);
    }

    #[tokio::test]
    async fn attachment_cleanup_of_a_deleted_chat_is_left_to_the_chat_cleanup() {
        let f = fixture().await;
        let res = f
            .app
            .call(
                "DELETE",
                &format!("{CHATS}/{}", f.chat),
                &ctx(f.tenant, f.user),
                None,
            )
            .await;
        assert_eq!(res.status, 204, "{}", res.json);
        assert!(matches!(
            f.handler().handle(&f.message(0)).await,
            MessageResult::Ok
        ));
        assert_eq!(f.deletes(), 0, "no provider call");
        let row = f.row().await;
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        assert_eq!(row.cleanup_attempts, 0);
    }

    #[tokio::test]
    async fn attachment_cleanup_without_a_provider_file_is_done_at_once() {
        let f = fixture().await;
        let mut ev = f.event();
        ev.provider_file_id = None;
        let msg = outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &ev, 0);
        assert!(matches!(f.handler().handle(&msg).await, MessageResult::Ok));
        assert!(f.app.gateway.requests().is_empty());
        assert_eq!(f.row().await.cleanup_status.as_deref(), Some("done"));
    }

    fn secondary_ref() -> crate::infra::outbox::SecondaryRef {
        crate::infra::outbox::SecondaryRef {
            file_id: "file_011sec".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: ANTHROPIC_ALIAS.to_owned(),
        }
    }

    const SECONDARY_PATH: &str = "/anthropic.test/v1/files/file_011sec";

    /// Like [`fixture`] with the `anthropic` provider configured (so a files client exists).
    async fn anthropic_fixture() -> Fixture {
        let app = TestApp::builder()
            .config(anthropic_config())
            .quiet_cleanup()
            .build()
            .await;
        fixture_in(app).await
    }

    #[tokio::test]
    async fn attachment_cleanup_deletes_the_secondary_file() {
        let f = anthropic_fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(200, json!({"deleted": true})),
        );
        f.app.gateway.on(
            Method::DELETE,
            SECONDARY_PATH,
            Responder::json(200, json!({"id": "file_011sec", "type": "file_deleted"})),
        );
        let mut ev = f.event();
        ev.secondary_ref = Some(secondary_ref());
        let msg = outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &ev, 0);
        assert!(matches!(f.handler().handle(&msg).await, MessageResult::Ok));
        assert_eq!(f.deletes(), 1);
        let secondary = f.app.gateway.requests_to(&Method::DELETE, SECONDARY_PATH);
        assert_eq!(secondary.len(), 1);
        assert_eq!(
            secondary[0]
                .headers
                .get("anthropic-beta")
                .map(|v| v.to_str().unwrap()),
            Some("files-api-2025-04-14")
        );
        assert_eq!(f.row().await.cleanup_status.as_deref(), Some("done"));

        // best effort: a failed secondary delete does not block the cleanup
        let f = anthropic_fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(200, json!({"deleted": true})),
        );
        f.app.gateway.on(
            Method::DELETE,
            SECONDARY_PATH,
            Responder::json(500, json!({})),
        );
        let mut ev = f.event();
        ev.secondary_ref = Some(secondary_ref());
        let msg = outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &ev, 0);
        assert!(matches!(f.handler().handle(&msg).await, MessageResult::Ok));
        assert_eq!(
            f.app
                .gateway
                .requests_to(&Method::DELETE, SECONDARY_PATH)
                .len(),
            1
        );
        assert_eq!(f.row().await.cleanup_status.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn attachment_cleanup_skips_the_secondary_file_without_a_files_client() {
        let f = fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &f.file_path(),
            Responder::json(200, json!({"deleted": true})),
        );
        let probe = MetricsProbe::new();
        let mut deps = CleanupDeps::from_services(&f.app.services);
        deps.metrics = Arc::clone(&probe.metrics);
        let mut ev = f.event();
        ev.secondary_ref = Some(secondary_ref());
        let msg = outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &ev, 0);
        assert!(matches!(
            AttachmentCleanupHandler::new(deps).handle(&msg).await,
            MessageResult::Ok
        ));
        assert_eq!(f.app.gateway.requests().len(), 1, "no secondary call");
        assert_eq!(f.row().await.cleanup_status.as_deref(), Some("done"));
        assert_eq!(
            probe.counter(
                "secondary_cleanup_skipped",
                &[("provider_kind", "anthropic")]
            ),
            1
        );
    }

    /// A failed delete whose outcome another delivery recorded while this one waited for the
    /// provider: the lost compare-and-set is resolved from the row as it is now.
    #[tokio::test]
    async fn attachment_cleanup_lost_cas_follows_the_observed_row() {
        for (concurrent_status, expected) in
            [("pending", "retry"), ("done", "ok"), ("failed", "reject")]
        {
            let f = fixture().await;
            f.app.gateway.on(
                Method::DELETE,
                &f.file_path(),
                Responder::Delayed(
                    Duration::from_millis(300),
                    Box::new(Responder::json(500, json!({}))),
                ),
            );
            let probe = MetricsProbe::new();
            let mut deps = CleanupDeps::from_services(&f.app.services);
            deps.metrics = Arc::clone(&probe.metrics);
            let handler = AttachmentCleanupHandler::new(deps);
            let msg = f.message(0);
            let delivery = tokio::spawn(async move { handler.handle(&msg).await });
            TestApp::wait_until("the provider delete is in flight", || async {
                f.deletes() == 1
            })
            .await;
            // Another delivery records its own failed attempt meanwhile.
            set_cleanup_state(&f.app, f.att, concurrent_status, 1).await;

            let res = delivery.await.expect("delivery");
            let label = match res {
                MessageResult::Ok => "ok",
                MessageResult::Retry => "retry",
                MessageResult::Reject(_) => "reject",
            };
            assert_eq!(label, expected, "concurrent {concurrent_status}");
            let row = f.row().await;
            assert_eq!(row.cleanup_status.as_deref(), Some(concurrent_status));
            assert_eq!(row.cleanup_attempts, 1, "the lost attempt is not recorded");
            // The delivery that recorded the transition counted it; this one counts nothing.
            for counter in ["cleanup_retry", "cleanup_failed", "cleanup_completed"] {
                assert_eq!(probe.counter(counter, &[]), 0, "{counter}");
            }
        }
    }

    #[tokio::test]
    async fn attachment_cleanup_rejects_a_malformed_payload() {
        let f = fixture().await;
        let msg = raw_outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, b"{\"tenant_id\":1}", 0);
        assert!(matches!(
            f.handler().handle(&msg).await,
            MessageResult::Reject(_)
        ));
    }

    #[tokio::test]
    async fn attachment_cleanup_retries_without_counting_when_s2s_is_missing() {
        let f = fixture().await;
        let mut deps = CleanupDeps::from_services(&f.app.services);
        deps.s2s = S2sContext::new(); // never set
        let handler = AttachmentCleanupHandler::new(deps);
        assert!(matches!(
            handler.handle(&f.message(0)).await,
            MessageResult::Retry
        ));
        assert_eq!(f.row().await.cleanup_attempts, 0);
        assert_eq!(f.deletes(), 0);
    }

    #[tokio::test]
    async fn attachment_cleanup_acks_a_row_that_is_not_pending() {
        let f = fixture().await;
        let mut ev = f.event();
        ev.attachment_id = Uuid::new_v4(); // unknown row
        let msg = outbox_message(ATTACHMENT_CLEANUP_PAYLOAD_TYPE, &ev, 0);
        assert!(matches!(f.handler().handle(&msg).await, MessageResult::Ok));
        assert_eq!(f.deletes(), 0);
    }
}
