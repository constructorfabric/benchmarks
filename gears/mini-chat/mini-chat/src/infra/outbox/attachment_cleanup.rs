//! Attachment cleanup handler (DESIGN section 4 "Attachment Deletion", Phase
//! 2; section 3.6 "Attachment cleanup state machine") and the per-attachment
//! provider delete shared with the chat cleanup handler.
//!
//! Handlers run as system jobs: tenant-scoped queries with the explicit
//! tenant and chat ids of the payload; provider calls go through
//! [`RagStorage`] with the gear's S2S identity.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use super::payloads::AttachmentCleanupPayload;
use crate::domain::enums::CleanupStatus;
use crate::domain::error::DomainError;
use crate::domain::ports::{RagStorage, StorageError};
use crate::domain::time::db_now;
use crate::infra::db::repos::attachment_repo::{self, CleanupFailure};
use crate::infra::db::repos::{chat_repo, tenant_scope};
use crate::infra::llm::ProviderResolver;

/// What the cleanup handlers need.
#[derive(Clone)]
pub struct CleanupDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub resolver: Arc<ProviderResolver>,
    pub storage: Arc<dyn RagStorage>,
    /// `cleanup_worker.max_attempts`.
    pub max_attempts: u32,
}

/// Outcome of one cleanup attempt on a `pending` attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileCleanup {
    /// Provider file gone (2xx, 404 or never uploaded); row `done`.
    Done,
    /// Failed attempt recorded; row stays `pending`.
    Retry,
    /// Failed attempt exhausted the budget; row `failed`.
    Failed,
    /// Nothing recorded: another worker changed the row meanwhile, or the
    /// request never reached the provider. Re-evaluate on the next delivery.
    Deferred,
}

/// Why a provider delete did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DeleteFailure {
    /// Never sent (S2S identity not ready): infrastructure, not an attempt.
    NotSent(String),
    /// A failed attempt (provider or gateway error, unknown backend label).
    Attempt(String),
}

/// 2xx and 404 are success; `Unavailable` was never sent.
pub(super) fn classify_delete(res: Result<(), StorageError>) -> Result<(), DeleteFailure> {
    match res {
        Ok(()) | Err(StorageError::NotFound) => Ok(()),
        Err(e @ StorageError::Unavailable(_)) => Err(DeleteFailure::NotSent(e.to_string())),
        Err(e) => Err(DeleteFailure::Attempt(e.to_string())),
    }
}

/// One `pending` attachment to clean.
pub(super) struct FileTarget<'a> {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<&'a str>,
    pub storage_backend: &'a str,
    /// `cleanup_attempts` as read.
    pub seen_attempts: i32,
    /// `provider_kind` of a secondary copy, when one exists.
    pub secondary_kind: Option<&'a str>,
}

impl CleanupDeps {
    /// Deletes the primary provider file (2xx and 404 are success), the
    /// secondary copy best effort, and records the outcome on the row.
    pub(super) async fn clean_file(&self, t: &FileTarget<'_>) -> Result<FileCleanup, DomainError> {
        if let Some(file_id) = t.provider_file_id {
            match self
                .delete_primary(t.tenant_id, t.storage_backend, file_id)
                .await
            {
                Ok(()) => {}
                Err(DeleteFailure::Attempt(error)) => return self.record_failure(t, &error).await,
                Err(DeleteFailure::NotSent(error)) => {
                    tracing::warn!(attachment_id = %t.attachment_id, %error, "provider file delete not sent; retrying");
                    return Ok(FileCleanup::Deferred);
                }
            }
            if let Some(kind) = t.secondary_kind {
                skip_secondary(t.attachment_id, kind);
            }
        }
        self.mark_done(t).await
    }

    async fn mark_done(&self, t: &FileTarget<'_>) -> Result<FileCleanup, DomainError> {
        let conn = self.db.conn()?;
        let n = attachment_repo::mark_cleanup_done(
            &conn,
            t.tenant_id,
            t.chat_id,
            t.attachment_id,
            db_now(),
        )
        .await?;
        if n == 1 {
            tracing::debug!(attachment_id = %t.attachment_id, "attachment provider cleanup done");
            Ok(FileCleanup::Done)
        } else {
            Ok(FileCleanup::Deferred)
        }
    }

    async fn delete_primary(
        &self,
        tenant_id: Uuid,
        backend: &str,
        file_id: &str,
    ) -> Result<(), DeleteFailure> {
        let st = self
            .resolver
            .storage_for_backend_label(backend, tenant_id)
            .map_err(|e| DeleteFailure::Attempt(e.to_string()))?;
        classify_delete(self.storage.delete_file(&st, file_id).await)
    }

    async fn record_failure(
        &self,
        t: &FileTarget<'_>,
        error: &str,
    ) -> Result<FileCleanup, DomainError> {
        let attempts = t.seen_attempts.saturating_add(1);
        let terminal = i64::from(attempts) >= i64::from(self.max_attempts);
        let conn = self.db.conn()?;
        let n = attachment_repo::record_cleanup_failure(
            &conn,
            t.tenant_id,
            t.chat_id,
            t.attachment_id,
            CleanupFailure {
                seen_attempts: t.seen_attempts,
                error,
                terminal,
            },
            db_now(),
        )
        .await?;
        if n == 0 {
            return Ok(FileCleanup::Deferred);
        }
        if terminal {
            tracing::error!(
                attachment_id = %t.attachment_id,
                attempts,
                %error,
                "attachment provider cleanup failed permanently"
            );
            Ok(FileCleanup::Failed)
        } else {
            tracing::warn!(
                attachment_id = %t.attachment_id,
                attempts,
                %error,
                "attachment provider delete failed; retrying"
            );
            Ok(FileCleanup::Retry)
        }
    }
}

/// Secondary (Anthropic) copy: no client for those files is wired in this
/// build, so the best-effort delete is skipped and logged.
fn skip_secondary(attachment_id: Uuid, provider_kind: &str) {
    tracing::warn!(
        %attachment_id,
        provider_kind,
        "secondary provider file cleanup skipped"
    );
}

/// `mini-chat.attachment_cleanup` handler.
pub struct AttachmentCleanupHandler {
    deps: CleanupDeps,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(deps: CleanupDeps) -> Self {
        Self { deps }
    }

    async fn process(&self, p: &AttachmentCleanupPayload) -> Result<MessageResult, DomainError> {
        let conn = self.deps.db.conn()?;
        let Some(chat) =
            chat_repo::find_any_scoped(&conn, &tenant_scope(p.tenant_id), p.chat_id).await?
        else {
            return Ok(MessageResult::Reject("chat not found".to_owned()));
        };
        if chat.deleted_at.is_some() {
            tracing::debug!(attachment_id = %p.attachment_id, "chat soft-deleted; chat cleanup owns the files");
            return Ok(MessageResult::Ok);
        }
        let Some(row) =
            attachment_repo::find_in_chat(&conn, p.tenant_id, p.chat_id, p.attachment_id).await?
        else {
            return Ok(MessageResult::Reject("attachment not found".to_owned()));
        };
        if row.cleanup_status.as_deref() != Some(CleanupStatus::Pending.as_str()) {
            tracing::debug!(
                attachment_id = %p.attachment_id,
                cleanup_status = ?row.cleanup_status,
                "attachment cleanup not pending; nothing to do"
            );
            return Ok(MessageResult::Ok);
        }
        let target = FileTarget {
            tenant_id: p.tenant_id,
            chat_id: p.chat_id,
            attachment_id: p.attachment_id,
            provider_file_id: p
                .provider_file_id
                .as_deref()
                .or(row.provider_file_id.as_deref()),
            storage_backend: &p.storage_backend,
            seen_attempts: row.cleanup_attempts,
            secondary_kind: p.secondary_ref.as_ref().map(|s| s.provider_kind.as_str()),
        };
        Ok(match self.deps.clean_file(&target).await? {
            FileCleanup::Done => MessageResult::Ok,
            FileCleanup::Retry | FileCleanup::Deferred => MessageResult::Retry,
            FileCleanup::Failed => MessageResult::Reject(format!(
                "provider file delete: max attempts ({}) reached",
                self.deps.max_attempts
            )),
        })
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(seq = msg.seq, error = %e, "malformed attachment cleanup payload");
                return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}"));
            }
        };
        match self.process(&p).await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(attachment_id = %p.attachment_id, error = %e, "attachment cleanup: database failure; retrying");
                MessageResult::Retry
            }
        }
    }
}
