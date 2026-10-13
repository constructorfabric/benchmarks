//! Chat cleanup handler: provider-side cleanup of a soft-deleted chat (DESIGN "Cleanup on Chat
//! Deletion"): first every `pending` attachment file, then the chat's vector store.

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::AccessScope;

use super::attachment_cleanup::{AttachmentFiles, CleanupDeps, FileOutcome, SecondaryFile, count};
use super::payloads::ChatCleanupEvent;
use crate::domain::error::DomainError;
use crate::infra::db::repo;
use crate::infra::db::repo::attachments::SECONDARY_PROVIDER_ANTHROPIC;
use crate::infra::db::tx::write_tx;

/// Cleans the provider resources of one soft-deleted chat.
///
/// - Chat not soft-deleted: `Reject("chat is not soft-deleted")`; malformed payload: `Reject`.
/// - Every `pending` attachment: delete the file (`done`, or an attempt is recorded and the row
///   becomes `failed` at `max_attempts`; the loop continues either way). Any attachment still
///   `pending` afterwards: `Retry`, the vector store waits.
/// - Then the `chat_vector_stores` row: delete the store (2xx / 404) and the row, even when some
///   attachments are `failed`. The delivery that removes the row counts `cleanup_completed`
///   and, with failed attachments, `cleanup_vector_store_with_failed_attachments`. A
///   failed delete is `Retry`, or `Reject` on the delivery that reaches `max_attempts` (every
///   delivery counts, the row is kept so a replay retries).
/// - Database errors and a missing S2S context: `Retry` without limit.
pub struct ChatCleanupHandler {
    deps: CleanupDeps,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(deps: CleanupDeps) -> Self {
        Self { deps }
    }

    async fn process(
        &self,
        ev: &ChatCleanupEvent,
        attempts: u32,
    ) -> Result<MessageResult, DomainError> {
        let deps = &self.deps;
        let scope = AccessScope::allow_all();
        let conn = deps.db.conn()?;
        let chat = repo::chats::find_any(&conn, &scope, ev.tenant_id, ev.chat_id).await?;
        match chat {
            None => return Ok(MessageResult::Reject("chat not found".to_owned())),
            Some(chat) if chat.deleted_at.is_none() => {
                return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned()));
            }
            Some(_) => {}
        }

        let pending =
            repo::attachments::pending_cleanup_in_chat(&conn, &scope, ev.tenant_id, ev.chat_id)
                .await?;
        let mut files_outstanding = false;
        for row in &pending {
            let secondary = row.secondary_file_id.clone().map(|file_id| SecondaryFile {
                file_id,
                provider_kind: row
                    .secondary_provider_kind
                    .clone()
                    .unwrap_or_else(|| SECONDARY_PROVIDER_ANTHROPIC.to_owned()),
                alias: deps.resolver.anthropic_files_alias(ev.tenant_id),
            });
            let outcome = deps
                .clean_attachment(AttachmentFiles {
                    tenant_id: ev.tenant_id,
                    id: row.id,
                    backend: &row.storage_backend,
                    file_id: row.provider_file_id.as_deref(),
                    attempts: row.cleanup_attempts,
                    secondary,
                })
                .await?;
            files_outstanding |= outcome == FileOutcome::Pending;
        }
        if files_outstanding {
            return Ok(MessageResult::Retry);
        }

        let Some(store) =
            repo::vector_stores::find_for_cleanup(&conn, &scope, ev.tenant_id, ev.chat_id).await?
        else {
            return Ok(MessageResult::Ok);
        };
        // A placeholder (creation never finished) has nothing at the provider.
        let deleted_at_provider = if let Some(vector_store_id) = &store.vector_store_id {
            deps.require_s2s()?;
            if let Err(reason) = deps
                .delete_vector_store(ev.tenant_id, &store.provider, vector_store_id)
                .await
            {
                tracing::warn!(chat_id = %ev.chat_id, %reason, "provider vector store delete failed");
                count(
                    &deps.metrics.cleanup_retry,
                    "vector_store",
                    Some("vector_store_delete_failed"),
                );
                if attempts.saturating_add(1) >= deps.max_attempts {
                    count(&deps.metrics.cleanup_failed, "vector_store", None);
                    return Ok(MessageResult::Reject(format!(
                        "vector store delete: max attempts ({}) reached",
                        deps.max_attempts
                    )));
                }
                return Ok(MessageResult::Retry);
            }
            true
        } else {
            false
        };
        let (tenant_id, chat_id, row_id) = (ev.tenant_id, ev.chat_id, store.id);
        let (removed, with_failed_attachments) = write_tx(&deps.db, |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                let failed =
                    repo::attachments::has_failed_cleanup_in_chat(tx, &scope, tenant_id, chat_id)
                        .await?;
                let removed =
                    repo::vector_stores::delete_row(tx, &scope, tenant_id, chat_id, row_id).await?;
                Ok((removed, failed))
            })
        })
        .await?;
        // Only the delivery that removed the row reports the deletion (a retried or overlapping
        // delivery may have called the provider too).
        if removed && deleted_at_provider {
            count(&deps.metrics.cleanup_completed, "vector_store", None);
            if with_failed_attachments {
                deps.metrics
                    .cleanup_vector_store_with_failed_attachments
                    .add(1, &[]);
            }
        }
        Ok(MessageResult::Ok)
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: ChatCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(err) => {
                return MessageResult::Reject(format!("malformed chat cleanup payload: {err}"));
            }
        };
        let attempts = u32::try_from(msg.attempts).unwrap_or(0);
        match self.process(&ev, attempts).await {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(chat_id = %ev.chat_id, error = %err, "chat cleanup deferred");
                MessageResult::Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use http::Method;
    use serde_json::json;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::infra::outbox::attachment_cleanup::CleanupDeps;
    use crate::infra::outbox::{CHAT_CLEANUP_PAYLOAD_TYPE, ChatCleanupEvent};
    use crate::test_support::app::{TestApp, anthropic_config, ctx};
    use crate::test_support::attachments::{
        PDF, VS_ID, attachment_row, delete_vector_store_rows, id_of, png, script_files,
        script_vector_store, seed_vector_store_row, set_cleanup_state, set_secondary_file, upload,
        vector_store_rows,
    };
    use crate::test_support::gateway::Responder;
    use crate::test_support::metrics::MetricsProbe;
    use crate::test_support::outbox::{outbox_message, raw_outbox_message};
    use crate::test_support::stream::{CHATS, SeedAttachment, create_chat, seed_attachment};

    const VS_PATH: &str = "/v1/vector_stores/vs_test1";

    fn file_path(file: &str) -> String {
        format!("/v1/files/{file}")
    }

    fn ok() -> Responder {
        Responder::json(200, json!({"deleted": true}))
    }

    /// An app whose queue handlers are quiet, so tests drive the chat cleanup handler by hand.
    async fn quiet_app() -> TestApp {
        TestApp::builder().quiet_cleanup().build().await
    }

    fn handler(app: &TestApp) -> ChatCleanupHandler {
        ChatCleanupHandler::new(CleanupDeps::from_services(&app.services))
    }

    fn message(tenant: Uuid, chat: Uuid, attempts: i16) -> OutboxMessage {
        let ev = ChatCleanupEvent {
            tenant_id: tenant,
            chat_id: chat,
            system_request_id: Uuid::nil(),
            reason: "chat_soft_delete".to_owned(),
            chat_deleted_at: OffsetDateTime::now_utc(),
        };
        outbox_message(CHAT_CLEANUP_PAYLOAD_TYPE, &ev, attempts)
    }

    async fn delete_chat(app: &TestApp, tenant: Uuid, user: Uuid, chat: Uuid) {
        let res = app
            .call(
                "DELETE",
                &format!("{CHATS}/{chat}"),
                &ctx(tenant, user),
                None,
            )
            .await;
        assert_eq!(res.status, 204, "{}", res.json);
    }

    /// A deleted chat with one seeded document and a seeded vector store `VS_ID`.
    struct Fixture {
        app: TestApp,
        tenant: Uuid,
        chat: Uuid,
        att: Uuid,
        file: String,
    }

    async fn fixture() -> Fixture {
        let app = quiet_app().await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), None).await;
        let att = seed_attachment(&app, SeedAttachment::document(tenant, chat, user)).await;
        seed_vector_store_row(&app, tenant, chat, Some(VS_ID), "openai").await;
        delete_chat(&app, tenant, user, chat).await;
        let file = format!("file-{}", att.simple());
        Fixture {
            app,
            tenant,
            chat,
            att,
            file,
        }
    }

    impl Fixture {
        async fn deliver(&self, attempts: i16) -> MessageResult {
            handler(&self.app)
                .handle(&message(self.tenant, self.chat, attempts))
                .await
        }

        fn vs_deletes(&self) -> usize {
            self.app.gateway.requests_to(&Method::DELETE, VS_PATH).len()
        }

        async fn status(&self) -> (Option<String>, i32) {
            let row = attachment_row(&self.app, self.chat, self.att).await;
            (row.cleanup_status, row.cleanup_attempts)
        }
    }

    #[tokio::test]
    async fn chat_cleanup_end_to_end() {
        let app = TestApp::builder().build().await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;
        script_files(&app, &["file-doc1", "file-img1"]);
        script_vector_store(
            &app,
            VS_ID,
            Duration::ZERO,
            json!({"id": "file-x", "object": "vector_store.file", "status": "completed"}),
        );
        let doc = upload(&app, &who, chat, "report.pdf", PDF, b"%PDF-1.4").await;
        assert_eq!(doc.status, 201, "{}", doc.json);
        let img = upload(&app, &who, chat, "photo.png", "image/png", &png(40, 30)).await;
        assert_eq!(img.status, 201, "{}", img.json);
        let (doc, img) = (id_of(&doc), id_of(&img));
        assert_eq!(vector_store_rows(&app, chat).await.len(), 1);
        for path in [
            file_path("file-doc1"),
            file_path("file-img1"),
            VS_PATH.to_owned(),
        ] {
            app.gateway.on(Method::DELETE, &path, ok());
        }

        delete_chat(&app, tenant, user, chat).await;

        TestApp::wait_until("the chat's vector store row is removed", || async {
            vector_store_rows(&app, chat).await.is_empty()
        })
        .await;
        for (path, n) in [
            (file_path("file-doc1"), 1),
            (file_path("file-img1"), 1),
            (VS_PATH.to_owned(), 1),
        ] {
            assert_eq!(
                app.gateway.requests_to(&Method::DELETE, &path).len(),
                n,
                "{path}"
            );
        }
        for id in [doc, img] {
            let row = attachment_row(&app, chat, id).await;
            assert_eq!(row.cleanup_status.as_deref(), Some("done"), "{id}");
            assert!(row.cleanup_updated_at.is_some());
        }
    }

    #[tokio::test]
    async fn chat_cleanup_waits_for_attachments_before_vector_store() {
        let f = fixture().await;
        f.app.gateway.on_sequence(
            Method::DELETE,
            &file_path(&f.file),
            vec![Responder::json(500, json!({})), ok()],
        );
        f.app.gateway.on(Method::DELETE, VS_PATH, ok());

        assert!(matches!(f.deliver(0).await, MessageResult::Retry));
        assert_eq!(f.status().await, (Some("pending".to_owned()), 1));
        assert_eq!(f.vs_deletes(), 0, "the store waits for the pending file");
        assert_eq!(vector_store_rows(&f.app, f.chat).await.len(), 1);

        assert!(matches!(f.deliver(1).await, MessageResult::Ok));
        assert_eq!(f.status().await, (Some("done".to_owned()), 1));
        assert_eq!(f.vs_deletes(), 1);
        assert!(vector_store_rows(&f.app, f.chat).await.is_empty());
    }

    #[tokio::test]
    async fn chat_cleanup_continues_past_a_terminally_failed_attachment() {
        let f = fixture().await;
        f.app.gateway.on(
            Method::DELETE,
            &file_path(&f.file),
            Responder::json(500, json!({})),
        );
        f.app.gateway.on(Method::DELETE, VS_PATH, ok());

        for attempt in 0..4 {
            assert!(matches!(f.deliver(attempt).await, MessageResult::Retry));
        }
        assert_eq!(f.vs_deletes(), 0);
        // The fifth failed delete exhausts the attachment; the store is then released.
        assert!(matches!(f.deliver(4).await, MessageResult::Ok));
        assert_eq!(f.status().await, (Some("failed".to_owned()), 5));
        assert_eq!(f.vs_deletes(), 1);
        assert!(vector_store_rows(&f.app, f.chat).await.is_empty());
    }

    #[tokio::test]
    async fn chat_cleanup_vector_store_failures_retry_then_reject_and_keep_the_row() {
        let f = fixture().await;
        f.app.gateway.on(Method::DELETE, &file_path(&f.file), ok());
        f.app
            .gateway
            .on(Method::DELETE, VS_PATH, Responder::json(500, json!({})));

        for attempt in 0..4 {
            assert!(matches!(f.deliver(attempt).await, MessageResult::Retry));
        }
        let res = f.deliver(4).await;
        assert!(
            matches!(&res, MessageResult::Reject(r) if r == "vector store delete: max attempts (5) reached"),
            "{res:?}"
        );
        assert_eq!(vector_store_rows(&f.app, f.chat).await.len(), 1, "row kept");
        assert_eq!(f.status().await.0.as_deref(), Some("done"));

        // A dead-letter replay retries the delete.
        f.app.gateway.on(Method::DELETE, VS_PATH, ok());
        assert!(matches!(f.deliver(0).await, MessageResult::Ok));
        assert!(vector_store_rows(&f.app, f.chat).await.is_empty());
    }

    #[tokio::test]
    async fn chat_cleanup_treats_a_missing_vector_store_as_deleted() {
        let f = fixture().await;
        f.app.gateway.on(Method::DELETE, &file_path(&f.file), ok());
        f.app
            .gateway
            .on(Method::DELETE, VS_PATH, Responder::json(404, json!({})));
        assert!(matches!(f.deliver(0).await, MessageResult::Ok));
        assert!(vector_store_rows(&f.app, f.chat).await.is_empty());
    }

    #[tokio::test]
    async fn chat_cleanup_drops_a_placeholder_without_a_provider_call() {
        let f = fixture().await;
        delete_vector_store_rows(&f.app, f.chat).await;
        seed_vector_store_row(&f.app, f.tenant, f.chat, None, "openai").await;
        f.app.gateway.on(Method::DELETE, &file_path(&f.file), ok());
        assert!(matches!(f.deliver(0).await, MessageResult::Ok));
        assert!(vector_store_rows(&f.app, f.chat).await.is_empty());
        assert_eq!(f.vs_deletes(), 0);
    }

    #[tokio::test]
    async fn chat_cleanup_without_a_vector_store_only_cleans_files() {
        let f = fixture().await;
        delete_vector_store_rows(&f.app, f.chat).await;
        f.app.gateway.on(Method::DELETE, &file_path(&f.file), ok());
        assert!(matches!(f.deliver(0).await, MessageResult::Ok));
        assert_eq!(f.status().await.0.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn chat_cleanup_deletes_secondary_files() {
        let app = TestApp::builder()
            .config(anthropic_config())
            .quiet_cleanup()
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), None).await;
        let att = seed_attachment(&app, SeedAttachment::image(tenant, chat, user)).await;
        set_secondary_file(&app, att, "file_011sec").await;
        delete_chat(&app, tenant, user, chat).await;
        let file = format!("file-{}", att.simple());
        app.gateway.on(Method::DELETE, &file_path(&file), ok());
        let secondary = "/anthropic.test/v1/files/file_011sec";
        app.gateway.on(Method::DELETE, secondary, ok());

        assert!(matches!(
            handler(&app).handle(&message(tenant, chat, 0)).await,
            MessageResult::Ok
        ));
        assert_eq!(app.gateway.requests_to(&Method::DELETE, secondary).len(), 1);
        let row = attachment_row(&app, chat, att).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    }

    /// Vector-store metrics are recorded by the delivery that removes the row, once.
    #[tokio::test]
    async fn chat_cleanup_vector_store_metrics_are_counted_once() {
        // a terminally failed attachment, a vector store delete that fails once
        let f = fixture().await;
        set_cleanup_state(&f.app, f.att, "failed", 5).await;
        f.app.gateway.on_sequence(
            Method::DELETE,
            VS_PATH,
            vec![Responder::json(500, json!({})), ok()],
        );
        let probe = MetricsProbe::new();
        let mut deps = CleanupDeps::from_services(&f.app.services);
        deps.metrics = Arc::clone(&probe.metrics);
        let handler = ChatCleanupHandler::new(deps);
        assert!(matches!(
            handler.handle(&message(f.tenant, f.chat, 0)).await,
            MessageResult::Retry
        ));
        assert!(matches!(
            handler.handle(&message(f.tenant, f.chat, 1)).await,
            MessageResult::Ok
        ));
        assert_eq!(
            probe.counter("cleanup_vector_store_with_failed_attachments", &[]),
            1
        );
        assert_eq!(
            probe.counter("cleanup_completed", &[("resource_type", "vector_store")]),
            1
        );

        // two overlapping deliveries of the same message both reach the provider
        let f = fixture().await;
        set_cleanup_state(&f.app, f.att, "done", 0).await;
        f.app.gateway.on(
            Method::DELETE,
            VS_PATH,
            Responder::Delayed(Duration::from_millis(200), Box::new(ok())),
        );
        let probe = MetricsProbe::new();
        let mut deps = CleanupDeps::from_services(&f.app.services);
        deps.metrics = Arc::clone(&probe.metrics);
        let handler = Arc::new(ChatCleanupHandler::new(deps));
        let deliveries: Vec<_> = (0..2)
            .map(|_| {
                let (handler, msg) = (Arc::clone(&handler), message(f.tenant, f.chat, 0));
                tokio::spawn(async move { handler.handle(&msg).await })
            })
            .collect();
        for delivery in deliveries {
            assert!(matches!(delivery.await.unwrap(), MessageResult::Ok));
        }
        assert_eq!(f.vs_deletes(), 2, "both deliveries called the provider");
        assert_eq!(
            probe.counter("cleanup_completed", &[("resource_type", "vector_store")]),
            1
        );
    }

    #[tokio::test]
    async fn chat_cleanup_rejects_active_chat() {
        let app = quiet_app().await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), None).await;
        seed_attachment(&app, SeedAttachment::document(tenant, chat, user)).await;
        let res = handler(&app).handle(&message(tenant, chat, 0)).await;
        assert!(
            matches!(&res, MessageResult::Reject(r) if r == "chat is not soft-deleted"),
            "{res:?}"
        );
        assert!(app.gateway.requests().is_empty());
    }

    #[tokio::test]
    async fn chat_cleanup_rejects_unknown_chat_and_malformed_payload() {
        let app = quiet_app().await;
        let res = handler(&app)
            .handle(&message(Uuid::new_v4(), Uuid::new_v4(), 0))
            .await;
        assert!(matches!(res, MessageResult::Reject(_)), "{res:?}");
        let garbage = raw_outbox_message(CHAT_CLEANUP_PAYLOAD_TYPE, b"nope", 0);
        assert!(matches!(
            handler(&app).handle(&garbage).await,
            MessageResult::Reject(_)
        ));
    }

    #[tokio::test]
    async fn chat_cleanup_does_not_touch_another_tenants_chat() {
        let f = fixture().await;
        let res = handler(&f.app)
            .handle(&message(Uuid::new_v4(), f.chat, 0))
            .await;
        assert!(matches!(res, MessageResult::Reject(_)), "{res:?}");
        assert!(f.app.gateway.requests().is_empty());
    }
}
