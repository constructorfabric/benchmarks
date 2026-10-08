//! Outbox handlers: usage publication, audit delivery, attachment cleanup
//! and chat cleanup.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginError, PublishError, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::Service;
use crate::domain::clock;
use crate::domain::events::{
    AttachmentCleanupEvent, ChatCleanupEvent, PAYLOAD_MUTATION_AUDIT, PAYLOAD_TURN_AUDIT,
};
use crate::infra::outbox::QueueHandler;
use crate::infra::plugins::audit_gateway::AuditPluginResolution;
use crate::infra::storage::entity::{attachment, chat, chat_vector_store};

/// Audit plugin call timeout.
pub const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Delivery attempt at which a retrying audit event is dead-lettered.
pub const AUDIT_MAX_ATTEMPTS: i16 = 120;

/// Usage queue handler.
pub struct UsageHandler(pub Arc<Service>);

#[async_trait]
impl QueueHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        match self.0.policy.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(reason)) => {
                tracing::warn!(%reason, "mini-chat: usage publication failed transiently");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(reason)) => MessageResult::Reject(reason),
        }
    }
}

enum AuditPayload {
    Turn(Box<TurnAuditEvent>),
    Mutation(TurnMutationAuditEvent),
}

/// Audit queue handler.
pub struct AuditHandler(pub Arc<Service>);

#[async_trait]
impl QueueHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload = match msg.payload_type.as_str() {
            PAYLOAD_TURN_AUDIT => serde_json::from_slice(&msg.payload)
                .map(|e: TurnAuditEvent| AuditPayload::Turn(Box::new(e))),
            PAYLOAD_MUTATION_AUDIT => {
                serde_json::from_slice(&msg.payload).map(AuditPayload::Mutation)
            }
            other => return MessageResult::Reject(format!("unknown audit payload type '{other}'")),
        };
        let payload = match payload {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed audit payload: {e}")),
        };
        let retry = |reason: String| {
            if msg.attempts.saturating_add(1) >= AUDIT_MAX_ATTEMPTS {
                MessageResult::Reject(format!("audit delivery gave up: {reason}"))
            } else {
                MessageResult::Retry
            }
        };
        let Some(gateway) = &self.0.audit else {
            return MessageResult::Ok;
        };
        let client = match gateway.resolve().await {
            AuditPluginResolution::Plugin(c) => c,
            AuditPluginResolution::NoPlugin => {
                self.0
                    .metrics
                    .audit_emit
                    .add(1, &crate::infra::metrics::labels(&[("result", "dropped")]));
                return MessageResult::Ok;
            }
            AuditPluginResolution::Retry(reason) => return retry(reason),
        };
        let m = &self.0.metrics;
        let emit = |result: &str| {
            m.audit_emit
                .add(1, &crate::infra::metrics::labels(&[("result", result)]));
        };
        let call = async {
            match payload {
                AuditPayload::Turn(e) => client.emit_turn_audit(*e).await,
                AuditPayload::Mutation(e) => client.emit_turn_mutation_audit(e).await,
            }
        };
        let out = match tokio::time::timeout(AUDIT_TIMEOUT, call).await {
            Ok(Ok(())) => MessageResult::Ok,
            Ok(Err(MiniChatAuditPluginError::Permanent(r))) => MessageResult::Reject(r),
            Ok(Err(e)) => retry(e.to_string()),
            Err(_) => retry("audit plugin timed out".to_owned()),
        };
        emit(match &out {
            MessageResult::Ok => "ok",
            MessageResult::Retry => "retry",
            MessageResult::Reject(_) => "reject",
        });
        out
    }
}

/// Attachment cleanup queue handler.
pub struct AttachmentCleanupHandler(pub Arc<Service>);

#[async_trait]
impl QueueHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed cleanup payload: {e}")),
        };
        self.0.attachment_cleanup(&event).await
    }
}

/// Chat cleanup queue handler.
pub struct ChatCleanupHandler(pub Arc<Service>);

#[async_trait]
impl QueueHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: ChatCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        self.0.chat_cleanup(&event, msg.attempts).await
    }
}

/// Result of one provider file delete attempt on a row.
enum FileOutcome {
    Done,
    /// Failed attempt recorded; the row is still pending.
    Pending,
    /// Failed attempt reached the limit; the row is terminal `failed`.
    Failed,
    /// Infrastructure failure (DB); retry without counting.
    Infra,
}

impl Service {
    async fn chat_is_deleted(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, ()> {
        let conn = self.db.conn().map_err(|_| ())?;
        let row = chat::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
            .one(&conn)
            .await
            .map_err(|_| ())?;
        Ok(row.is_none_or(|c| c.deleted_at.is_some()))
    }

    async fn mark_cleanup(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        status: &str,
        error: Option<String>,
        attempt: bool,
    ) -> bool {
        let Ok(conn) = self.db.conn() else {
            return false;
        };
        let mut q = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(Some(status.to_owned())),
            )
            .col_expr(
                attachment::Column::CleanupUpdatedAt,
                Expr::value(Some(clock::now())),
            );
        if let Some(e) = error {
            q = q.col_expr(attachment::Column::LastCleanupError, Expr::value(Some(e)));
        }
        if attempt {
            q = q.col_expr(
                attachment::Column::CleanupAttempts,
                sea_orm::ExprTrait::add(Expr::col(attachment::Column::CleanupAttempts), 1),
            );
        }
        q.filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .is_ok()
    }

    /// Delete one attachment's provider file and record the outcome.
    async fn cleanup_file(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        file_id: Option<&str>,
        storage_backend: &str,
        attempts_so_far: i32,
    ) -> FileOutcome {
        let Some(file_id) = file_id else {
            return if self
                .mark_cleanup(tenant_id, attachment_id, "done", None, false)
                .await
            {
                FileOutcome::Done
            } else {
                FileOutcome::Infra
            };
        };
        let Some(target) = self.providers.storage_by_label(storage_backend, tenant_id) else {
            let err = format!("unknown storage backend '{storage_backend}'");
            return self
                .record_failure(tenant_id, attachment_id, err, attempts_so_far)
                .await;
        };
        match self.storage.delete_file(&target, file_id).await {
            Ok(()) => {
                self.metrics.cleanup_completed.add(
                    1,
                    &crate::infra::metrics::labels(&[("resource_type", "file")]),
                );
                if self
                    .mark_cleanup(tenant_id, attachment_id, "done", None, false)
                    .await
                {
                    FileOutcome::Done
                } else {
                    FileOutcome::Infra
                }
            }
            Err(e) => {
                self.record_failure(tenant_id, attachment_id, e.to_string(), attempts_so_far)
                    .await
            }
        }
    }

    async fn record_failure(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        err: String,
        attempts_so_far: i32,
    ) -> FileOutcome {
        let max = i32::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        let file = crate::infra::metrics::labels(&[("resource_type", "file")]);
        if attempts_so_far + 1 >= max {
            self.metrics.cleanup_failed.add(1, &file);
        } else {
            self.metrics.cleanup_retry.add(
                1,
                &crate::infra::metrics::labels(&[
                    ("resource_type", "file"),
                    ("reason", "provider_error"),
                ]),
            );
        }
        if attempts_so_far + 1 >= max {
            if self
                .mark_cleanup(tenant_id, id, "failed", Some(err), true)
                .await
            {
                FileOutcome::Failed
            } else {
                FileOutcome::Infra
            }
        } else if self
            .mark_cleanup(tenant_id, id, "pending", Some(err), true)
            .await
        {
            FileOutcome::Pending
        } else {
            FileOutcome::Infra
        }
    }

    async fn delete_secondary(&self, tenant_id: Uuid, alias: &str, file_id: &str) {
        let _ = tenant_id;
        if let Err(e) = self.storage.delete_anthropic_file(alias, file_id).await {
            tracing::warn!(error = %e, "mini-chat: secondary file delete failed");
        }
    }

    /// Attachment cleanup (deleted attachment, abandoned upload, failed
    /// background indexing).
    pub(crate) async fn attachment_cleanup(&self, ev: &AttachmentCleanupEvent) -> MessageResult {
        match self.chat_is_deleted(ev.tenant_id, ev.chat_id).await {
            Ok(true) => return MessageResult::Ok,
            Ok(false) => {}
            Err(()) => return MessageResult::Retry,
        }
        let row = {
            let Ok(conn) = self.db.conn() else {
                return MessageResult::Retry;
            };
            match attachment::Entity::find()
                .secure()
                .scope_with(&AccessScope::for_tenant(ev.tenant_id))
                .filter(Condition::all().add(attachment::Column::Id.eq(ev.attachment_id)))
                .one(&conn)
                .await
            {
                Ok(r) => r,
                Err(_) => return MessageResult::Retry,
            }
        };
        let Some(row) = row else {
            return MessageResult::Ok;
        };
        if matches!(row.cleanup_status.as_deref(), Some("done" | "failed")) {
            return MessageResult::Ok;
        }
        let outcome = self
            .cleanup_file(
                ev.tenant_id,
                ev.attachment_id,
                ev.provider_file_id.as_deref(),
                &ev.storage_backend,
                row.cleanup_attempts,
            )
            .await;
        if matches!(outcome, FileOutcome::Done)
            && let Some(sec) = &ev.secondary_ref
        {
            self.delete_secondary(ev.tenant_id, &sec.upstream_alias, &sec.file_id)
                .await;
        }
        match outcome {
            FileOutcome::Done => MessageResult::Ok,
            FileOutcome::Pending | FileOutcome::Infra => MessageResult::Retry,
            FileOutcome::Failed => MessageResult::Reject(format!(
                "attachment cleanup: max attempts ({}) reached",
                self.cfg.cleanup_worker.max_attempts
            )),
        }
    }

    /// Chat cleanup: provider files of pending attachments, then the vector
    /// store once every attachment is terminal.
    pub(crate) async fn chat_cleanup(&self, ev: &ChatCleanupEvent, attempts: i16) -> MessageResult {
        match self.chat_is_deleted(ev.tenant_id, ev.chat_id).await {
            Ok(true) => {}
            Ok(false) => return MessageResult::Reject("chat is not soft-deleted".to_owned()),
            Err(()) => return MessageResult::Retry,
        }
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let pending = {
            let Ok(conn) = self.db.conn() else {
                return MessageResult::Retry;
            };
            match attachment::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(ev.chat_id))
                        .add(attachment::Column::CleanupStatus.eq("pending")),
                )
                .all(&conn)
                .await
            {
                Ok(r) => r,
                Err(_) => return MessageResult::Retry,
            }
        };
        let mut still_pending = false;
        for a in pending {
            let outcome = self
                .cleanup_file(
                    ev.tenant_id,
                    a.id,
                    a.provider_file_id.as_deref(),
                    &a.storage_backend,
                    a.cleanup_attempts,
                )
                .await;
            if matches!(outcome, FileOutcome::Done)
                && let (Some(fid), "uploaded") = (&a.secondary_file_id, a.secondary_status.as_str())
                && let Some(alias) = self.anthropic_alias_for_chat(ev.tenant_id)
            {
                self.delete_secondary(ev.tenant_id, &alias, fid).await;
            }
            if matches!(outcome, FileOutcome::Pending | FileOutcome::Infra) {
                still_pending = true;
            }
        }
        if still_pending {
            return MessageResult::Retry;
        }
        // Vector store.
        let store = {
            let Ok(conn) = self.db.conn() else {
                return MessageResult::Retry;
            };
            match chat_vector_store::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(ev.chat_id)))
                .one(&conn)
                .await
            {
                Ok(r) => r,
                Err(_) => return MessageResult::Retry,
            }
        };
        let Some(store) = store else {
            return MessageResult::Ok;
        };
        let deleted = match (
            &store.vector_store_id,
            self.providers
                .storage_by_label(&store.provider, ev.tenant_id),
        ) {
            (None, _) => Ok(()),
            (Some(vs), Some(target)) => self
                .storage
                .delete_vector_store(&target, vs)
                .await
                .map_err(|e| e.to_string()),
            (Some(_), None) => Err(format!("unknown storage backend '{}'", store.provider)),
        };
        match deleted {
            Ok(()) => {
                let Ok(conn) = self.db.conn() else {
                    return MessageResult::Retry;
                };
                match chat_vector_store::Entity::delete_many()
                    .filter(Condition::all().add(chat_vector_store::Column::Id.eq(store.id)))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await
                {
                    Ok(_) => MessageResult::Ok,
                    Err(_) => MessageResult::Retry,
                }
            }
            Err(e) => {
                let max = i16::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i16::MAX);
                if attempts.saturating_add(1) >= max {
                    MessageResult::Reject(format!(
                        "vector store delete: max attempts ({max}) reached: {e}"
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }

    fn anthropic_alias_for_chat(&self, tenant_id: Uuid) -> Option<String> {
        self.providers
            .entries()
            .into_iter()
            .find(|e| e.entry.kind == crate::config::ProviderKind::AnthropicMessages)
            .and_then(|e| self.providers.anthropic_alias(&e.id, tenant_id))
    }
}
