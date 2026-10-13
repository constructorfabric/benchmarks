//! Outbox cleanup handlers (attachment / chat), upload reaper and orphan watchdog scans.

use crate::infra::db::WriteTransaction;
use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use super::attachments::AttachmentCleanupPayload;
use super::chats::{ChatCleanupPayload, tenant_scope};
use super::summary::TaskOutcome;
use super::{Core, now};
use crate::infra::db::entities::{attachment, chat, turn, vector_store};
use crate::infra::llm::storage::DeleteOutcome;
use crate::infra::outbox::QueueKind;

/// Result of one provider file delete attempt for an attachment row.
enum FileCleanup {
    Done,
    Failed { terminal: bool },
}

impl Core {
    async fn cleanup_attachment_file(&self, a: &attachment::Model) -> Result<FileCleanup, String> {
        let scope = tenant_scope(a.tenant_id);
        let conn = self.db.conn().map_err(|e| e.to_string())?;
        let ts = now();
        let outcome = match &a.provider_file_id {
            None => Ok(()),
            Some(file_id) => {
                let provider = self
                    .providers
                    .resolve_backend(&a.storage_backend, a.tenant_id)
                    .map_err(|e| e.to_string())?;
                let ctx = self.system_ctx.get_or_tenant(a.tenant_id);
                match self.storage.delete_file(&ctx, &provider, file_id).await {
                    Ok(DeleteOutcome::Deleted | DeleteOutcome::NotFound) => Ok(()),
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        match outcome {
            Ok(()) => {
                attachment::Entity::update_many()
                    .col_expr(
                        attachment::Column::CleanupStatus,
                        Expr::value(Some("done".to_owned())),
                    )
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                    .filter(attachment::Column::Id.eq(a.id))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(FileCleanup::Done)
            }
            Err(err) => {
                let attempts = a.cleanup_attempts + 1;
                let terminal = u32::try_from(attempts).unwrap_or(u32::MAX)
                    >= self.cfg.cleanup_worker.max_attempts;
                let mut upd = attachment::Entity::update_many()
                    .col_expr(attachment::Column::CleanupAttempts, Expr::value(attempts))
                    .col_expr(attachment::Column::LastCleanupError, Expr::value(Some(err)))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
                if terminal {
                    upd = upd.col_expr(
                        attachment::Column::CleanupStatus,
                        Expr::value(Some("failed".to_owned())),
                    );
                }
                upd.filter(attachment::Column::Id.eq(a.id))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(FileCleanup::Failed { terminal })
            }
        }
    }

    /// Attachment cleanup handler (delete / upload-abandoned / indexing-failed events).
    pub async fn process_attachment_cleanup(&self, p: &AttachmentCleanupPayload) -> TaskOutcome {
        let scope = tenant_scope(p.tenant_id);
        let Ok(conn) = self.db.conn() else {
            return TaskOutcome::Retry("db".to_owned());
        };
        match chat::Entity::find()
            .filter(chat::Column::Id.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(Some(c)) if c.deleted_at.is_some() => return TaskOutcome::Ok,
            Ok(_) => {}
            Err(e) => return TaskOutcome::Retry(e.to_string()),
        }
        let row = match attachment::Entity::find()
            .filter(attachment::Column::Id.eq(p.attachment_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => return TaskOutcome::Ok,
            Err(e) => return TaskOutcome::Retry(e.to_string()),
        };
        if matches!(row.cleanup_status.as_deref(), Some("done" | "failed")) {
            return TaskOutcome::Ok;
        }
        match self.cleanup_attachment_file(&row).await {
            Ok(FileCleanup::Done) => TaskOutcome::Ok,
            Ok(FileCleanup::Failed { terminal: true }) => {
                TaskOutcome::Reject("attachment cleanup: max attempts reached".to_owned())
            }
            Ok(FileCleanup::Failed { terminal: false }) => {
                TaskOutcome::Retry("provider delete failed".to_owned())
            }
            Err(e) => TaskOutcome::Retry(e),
        }
    }

    /// Chat cleanup handler: provider files, then the chat vector store.
    pub async fn process_chat_cleanup(&self, p: &ChatCleanupPayload, delivery: u32) -> TaskOutcome {
        let scope = tenant_scope(p.tenant_id);
        let Ok(conn) = self.db.conn() else {
            return TaskOutcome::Retry("db".to_owned());
        };
        match chat::Entity::find()
            .filter(chat::Column::Id.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return TaskOutcome::Reject("chat is not soft-deleted".to_owned()),
            Err(e) => return TaskOutcome::Retry(e.to_string()),
        }
        let pending = match attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(p.chat_id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            )
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
        {
            Ok(r) => r,
            Err(e) => return TaskOutcome::Retry(e.to_string()),
        };
        let mut still_pending = false;
        for a in &pending {
            match self.cleanup_attachment_file(a).await {
                Ok(FileCleanup::Done | FileCleanup::Failed { terminal: true }) => {}
                Ok(FileCleanup::Failed { terminal: false }) | Err(_) => still_pending = true,
            }
        }
        let max = self.cfg.cleanup_worker.max_attempts;
        if still_pending {
            return TaskOutcome::Retry("attachment cleanup pending".to_owned());
        }
        let Ok(conn) = self.db.conn() else {
            return TaskOutcome::Retry("db".to_owned());
        };
        let vs = match vector_store::Entity::find()
            .filter(
                Condition::all()
                    .add(vector_store::Column::ChatId.eq(p.chat_id))
                    .add(vector_store::Column::TenantId.eq(p.tenant_id)),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(v) => v,
            Err(e) => return TaskOutcome::Retry(e.to_string()),
        };
        let Some(vs) = vs else { return TaskOutcome::Ok };
        let deleted = match &vs.vector_store_id {
            None => true,
            Some(vs_id) => match self.providers.resolve_backend(&vs.provider, p.tenant_id) {
                Ok(provider) => {
                    let ctx = self.system_ctx.get_or_tenant(p.tenant_id);
                    self.storage
                        .delete_vector_store(&ctx, &provider, vs_id)
                        .await
                        .is_ok()
                }
                Err(_) => false,
            },
        };
        if deleted {
            if let Err(e) = vector_store::Entity::delete_many()
                .filter(vector_store::Column::Id.eq(vs.id))
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await
            {
                return TaskOutcome::Retry(e.to_string());
            }
            return TaskOutcome::Ok;
        }
        if delivery >= max {
            TaskOutcome::Reject(format!("vector store delete: max attempts ({max}) reached"))
        } else {
            TaskOutcome::Retry("vector store delete failed".to_owned())
        }
    }

    /// One upload-reaper scan; returns the number of rows failed.
    pub async fn reap_abandoned_uploads(self: &Arc<Self>) -> usize {
        let cutoff = now()
            - time::Duration::seconds(
                i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300),
            );
        let Ok(conn) = self.db.conn() else { return 0 };
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by(attachment::Column::UpdatedAt, Order::Asc)
            .limit(100)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap_or_default();
        let mut n = 0;
        for row in rows {
            let core = Arc::clone(self);
            let res = self
                .db
                .write_transaction(move |tx| {
                    Box::pin(async move {
                        let ts = now();
                        let scope = tenant_scope(row.tenant_id);
                        let mut upd = attachment::Entity::update_many()
                            .col_expr(attachment::Column::Status, Expr::value("failed"))
                            .col_expr(
                                attachment::Column::ErrorCode,
                                Expr::value(Some("upload_abandoned".to_owned())),
                            )
                            .col_expr(attachment::Column::UpdatedAt, Expr::value(ts));
                        if row.provider_file_id.is_some() {
                            upd = upd
                                .col_expr(
                                    attachment::Column::CleanupStatus,
                                    Expr::value(Some("pending".to_owned())),
                                )
                                .col_expr(
                                    attachment::Column::CleanupUpdatedAt,
                                    Expr::value(Some(ts)),
                                );
                        }
                        let changed = upd
                            .filter(
                                Condition::all()
                                    .add(attachment::Column::Id.eq(row.id))
                                    .add(attachment::Column::Status.eq(row.status.clone()))
                                    .add(attachment::Column::DeletedAt.is_null())
                                    .add(attachment::Column::CleanupStatus.is_null())
                                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
                            )
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await?
                            .rows_affected;
                        if changed == 0 {
                            return Ok((false, Wake::empty()));
                        }
                        if row.provider_file_id.is_some() {
                            let payload = AttachmentCleanupPayload::from_row(
                                &row,
                                "attachment_upload_abandoned",
                                ts,
                            );
                            let w = core
                                .outbox
                                .enqueue(tx, QueueKind::AttachmentCleanup, row.tenant_id, &payload)
                                .await?;
                            return Ok((true, w));
                        }
                        Ok((true, Wake::empty()))
                    })
                })
                .await;
            if let Ok((changed, w)) = res {
                w.fire();
                if changed {
                    n += 1;
                }
            }
        }
        n
    }

    /// One orphan-watchdog scan; returns the number of turns finalized.
    pub async fn scan_orphans(self: &Arc<Self>) -> usize {
        let cutoff: OffsetDateTime = now()
            - time::Duration::seconds(
                i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300),
            );
        let Ok(conn) = self.db.conn() else { return 0 };
        let rows = turn::Entity::find()
            .filter(
                Condition::all()
                    .add(turn::Column::State.eq("running"))
                    .add(turn::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(turn::Column::LastProgressAt.lte(cutoff))
                            .add(
                                Condition::all()
                                    .add(turn::Column::LastProgressAt.is_null())
                                    .add(turn::Column::StartedAt.lte(cutoff)),
                            ),
                    ),
            )
            .limit(100)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap_or_default();
        let mut n = 0;
        for t in rows {
            match self.finalize_orphan(t, cutoff).await {
                Ok(true) => n += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "mini-chat: orphan finalization failed"),
            }
        }
        n
    }
}
