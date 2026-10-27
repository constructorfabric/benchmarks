//! Attachment repository.

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::attachments;
use crate::error::DomainError;

/// Attachment statuses.
pub mod status {
    /// Upload started.
    pub const PENDING: &str = "pending";
    /// Provider upload done, indexing not finished.
    pub const UPLOADED: &str = "uploaded";
    /// Ready.
    pub const READY: &str = "ready";
    /// Failed.
    pub const FAILED: &str = "failed";
}

/// Cleanup statuses (`attachments.cleanup_status`).
pub mod cleanup {
    /// Provider cleanup outstanding.
    pub const PENDING: &str = "pending";
    /// Provider cleanup done.
    pub const DONE: &str = "done";
    /// Provider cleanup failed terminally.
    pub const FAILED: &str = "failed";
}

/// New attachment row.
pub struct NewAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Uploader.
    pub uploaded_by_user_id: Uuid,
    /// Filename.
    pub filename: String,
    /// MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Storage routing label.
    pub storage_backend: String,
    /// `document` | `image`.
    pub attachment_kind: String,
    /// Routed to file search.
    pub for_file_search: bool,
    /// Routed to code interpreter.
    pub for_code_interpreter: bool,
}

/// Attachment repository.
#[derive(Clone)]
pub struct AttachmentRepo {
    #[allow(dead_code)]
    db: Db,
}

impl AttachmentRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn base_select(
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> toolkit_db::secure::SecureSelect<attachments::Entity, toolkit_db::secure::Scoped> {
        attachments::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat_id)))
    }

    /// Insert an attachment in `pending` status.
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        new: NewAttachment,
    ) -> Result<attachments::Model, DomainError> {
        let now = now();
        let am = attachments::ActiveModel {
            id: ActiveValue::Set(new.id),
            tenant_id: ActiveValue::Set(new.tenant_id),
            chat_id: ActiveValue::Set(new.chat_id),
            uploaded_by_user_id: ActiveValue::Set(new.uploaded_by_user_id),
            filename: ActiveValue::Set(new.filename),
            content_type: ActiveValue::Set(new.content_type),
            size_bytes: ActiveValue::Set(new.size_bytes),
            storage_backend: ActiveValue::Set(new.storage_backend),
            provider_file_id: ActiveValue::Set(None),
            status: ActiveValue::Set(status::PENDING.to_owned()),
            error_code: ActiveValue::Set(None),
            attachment_kind: ActiveValue::Set(new.attachment_kind),
            for_file_search: ActiveValue::Set(new.for_file_search),
            for_code_interpreter: ActiveValue::Set(new.for_code_interpreter),
            doc_summary: ActiveValue::Set(None),
            img_thumbnail: ActiveValue::Set(None),
            img_thumbnail_width: ActiveValue::Set(None),
            img_thumbnail_height: ActiveValue::Set(None),
            summary_model: ActiveValue::Set(None),
            summary_updated_at: ActiveValue::Set(None),
            cleanup_status: ActiveValue::Set(None),
            cleanup_attempts: ActiveValue::Set(0),
            last_cleanup_error: ActiveValue::Set(None),
            cleanup_updated_at: ActiveValue::Set(None),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
            deleted_at: ActiveValue::Set(None),
            secondary_file_id: ActiveValue::Set(None),
            secondary_status: ActiveValue::Set("not_attempted".to_owned()),
            secondary_provider_kind: ActiveValue::Set(None),
        };
        let model = toolkit_db::secure::secure_insert::<attachments::Entity>(am, &super::tenant_scope(scope), runner).await?;
        Ok(model)
    }

    /// Load an attachment by id (not deleted).
    pub async fn get(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<attachments::Model>, DomainError> {
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Load an attachment by id, including soft-deleted rows.
    pub async fn get_any(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<attachments::Model>, DomainError> {
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Load an attachment by id only (cleanup handlers).
    pub async fn get_any_raw(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
    ) -> Result<attachments::Model, DomainError> {
        let row = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .one(runner)
            .await?
            .ok_or(DomainError::NotFound(crate::error::ResourceKind::Attachment))?;
        Ok(row)
    }

    /// Mark the provider upload done (`pending` -> `uploaded` with the
    /// provider file id).
    pub async fn mark_uploaded(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        provider_file_id: &str,
    ) -> Result<(), DomainError> {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                Expr::value(status::UPLOADED.to_owned()),
            )
            .col_expr(
                attachments::Column::ProviderFileId,
                Expr::value(Some(provider_file_id.to_owned())),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Record the secondary (Anthropic) upload outcome.
    pub async fn set_secondary(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        file_id: Option<&str>,
        status: &str,
    ) -> Result<(), DomainError> {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::SecondaryFileId,
                Expr::value(file_id.map(str::to_owned)),
            )
            .col_expr(
                attachments::Column::SecondaryStatus,
                Expr::value(status.to_owned()),
            )
            .col_expr(
                attachments::Column::SecondaryProviderKind,
                Expr::value(Some("anthropic".to_owned())),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Mark `ready` (optionally with a thumbnail). The background-indexing
    /// variant additionally requires `cleanup_status IS NULL` and returns
    /// whether the update happened.
    pub async fn mark_ready(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        thumbnail: Option<(Vec<u8>, i32, i32)>,
        require_no_cleanup: bool,
    ) -> Result<bool, DomainError> {
        let (bytes, w, h) = match thumbnail {
            Some((b, w, h)) => (Some(b), Some(w), Some(h)),
            None => (None, None, None),
        };
        let mut update = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                Expr::value(status::READY.to_owned()),
            )
            .col_expr(attachments::Column::ImgThumbnail, Expr::value(bytes))
            .col_expr(attachments::Column::ImgThumbnailWidth, Expr::value(w))
            .col_expr(attachments::Column::ImgThumbnailHeight, Expr::value(h))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()));
        if require_no_cleanup {
            update = update
                .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()));
        }
        let res = update
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Mark `failed` with an error code.
    pub async fn mark_failed(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        error_code: &str,
    ) -> Result<(), DomainError> {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                Expr::value(status::FAILED.to_owned()),
            )
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(Some(error_code.to_owned())),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Background-indexing failure: `failed` + `indexing_failed` +
    /// `cleanup_status = pending` (the outbox event is enqueued by the
    /// caller in the same transaction).
    pub async fn fail_indexing(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let res = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                Expr::value(status::FAILED.to_owned()),
            )
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(Some("indexing_failed".to_owned())),
            )
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(cleanup::PENDING.to_owned())),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(
                Condition::all().add(attachments::Column::Status.eq(status::UPLOADED)),
            )
            .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Refresh `updated_at` while the row is still `uploaded` and not marked
    /// for cleanup (background indexing heartbeat).
    pub async fn heartbeat(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let res = attachments::Entity::update_many()
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(Condition::all().add(attachments::Column::Status.eq(status::UPLOADED)))
            .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Soft-delete an attachment and mark cleanup pending.
    pub async fn soft_delete(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = attachments::Entity::update_many()
            .col_expr(attachments::Column::DeletedAt, Expr::value(Some(now)))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(cleanup::PENDING.to_owned())),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(Condition::all().add(attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Non-deleted, non-failed document count of a chat.
    pub async fn count_documents(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<u64, DomainError> {
        let n = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::Status.ne(status::FAILED)))
            .filter(Condition::all().add(attachments::Column::AttachmentKind.eq("document")))
            .count(runner)
            .await?;
        Ok(n)
    }

    /// Total size of non-deleted, non-failed attachments of a chat (images
    /// included).
    pub async fn total_bytes(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<i64, DomainError> {
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct SumRow {
            total: Option<i64>,
        }
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::Status.ne(status::FAILED)))
            .project_all(runner, |q| {
                q.select_only()
                    .column_as(attachments::Column::SizeBytes.sum(), "total")
                    .into_model::<SumRow>()
            })
            .await?;
        Ok(row.first().and_then(|r| r.total).unwrap_or(0))
    }

    /// Whether the chat has at least one ready document attachment.
    pub async fn has_ready_document(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        let n = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::Status.eq(status::READY)))
            .filter(Condition::all().add(attachments::Column::ForFileSearch.eq(true)))
            .limit(1)
            .all(runner)
            .await?;
        Ok(!n.is_empty())
    }

    /// Ready code-interpreter provider file ids of a chat.
    pub async fn ready_code_interpreter_files(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<String>, DomainError> {
        let rows = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::Status.eq(status::READY)))
            .filter(Condition::all().add(attachments::Column::ForCodeInterpreter.eq(true)))
            .all(runner)
            .await?;
        Ok(rows.into_iter().filter_map(|r| r.provider_file_id).collect())
    }

    /// Ready, non-deleted attachments with a provider file id (citation map).
    pub async fn ready_with_provider_ids(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<attachments::Model>, DomainError> {
        let rows = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::Status.eq(status::READY)))
            .filter(Condition::all().add(attachments::Column::ProviderFileId.is_not_null()))
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Filter a list of ids to those that exist and are not deleted
    /// (mutation copy path).
    pub async fn filter_existing(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<Uuid>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(attachments::Column::Id.is_in(ids.iter().copied())))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .all(runner)
            .await?;
        let existing: std::collections::HashSet<Uuid> = rows.into_iter().map(|r| r.id).collect();
        Ok(ids.iter().copied().filter(|id| existing.contains(id)).collect())
    }

    /// Fetch attachments by ids (any status, not deleted) for
    /// `attachment_ids` validation.
    pub async fn get_many(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<attachments::Model>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(attachments::Column::Id.is_in(ids.iter().copied())))
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Mark every attachment of a soft-deleted chat as cleanup-pending.
    pub async fn mark_cleanup_pending_for_chat(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let now = now();
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(cleanup::PENDING.to_owned())),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Attachments of a chat with `cleanup_status = pending` (chat cleanup
    /// handler; tenant-scoped, includes soft-deleted rows).
    pub async fn list_cleanup_pending(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<attachments::Model>, DomainError> {
        let rows = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat_id)))
            .filter(
                Condition::all().add(attachments::Column::CleanupStatus.eq(cleanup::PENDING)),
            )
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Record a failed provider-delete attempt.
    pub async fn cleanup_record_failure(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        error: &str,
    ) -> Result<(), DomainError> {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupAttempts,
                Expr::col(attachments::Column::CleanupAttempts).add(1),
            )
            .col_expr(
                attachments::Column::LastCleanupError,
                Expr::value(Some(error.to_owned())),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now())))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Mark cleanup `done` or terminally `failed`.
    pub async fn cleanup_set_terminal(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        terminal: &str,
        error: Option<&str>,
    ) -> Result<(), DomainError> {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(terminal.to_owned())),
            )
            .col_expr(
                attachments::Column::LastCleanupError,
                Expr::value(error.map(str::to_owned)),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now())))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Upload reaper scan: stale `pending`/`uploaded` rows not owned by chat
    /// cleanup.
    pub async fn reaper_scan(
        &self,
        runner: &impl DBRunner,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<attachments::Model>, DomainError> {
        let rows = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(attachments::Column::Status.is_in([status::PENDING, status::UPLOADED])),
            )
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()))
            .filter(Condition::all().add(attachments::Column::UpdatedAt.lt(cutoff)))
            .order_by(attachments::Column::UpdatedAt, sea_orm::Order::Asc)
            .limit(limit)
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Upload reaper CAS: fail a stale row (`upload_abandoned`), returning
    /// whether this worker won.
    pub async fn reaper_fail(
        &self,
        runner: &impl DBRunner,
        id: Uuid,
        cutoff: OffsetDateTime,
        has_provider_file: bool,
    ) -> Result<bool, DomainError> {
        let now = now();
        let mut update = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                Expr::value(status::FAILED.to_owned()),
            )
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(Some("upload_abandoned".to_owned())),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .filter(
                Condition::all()
                    .add(attachments::Column::Status.is_in([status::PENDING, status::UPLOADED])),
            )
            .filter(Condition::all().add(attachments::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(attachments::Column::CleanupStatus.is_null()))
            .filter(Condition::all().add(attachments::Column::UpdatedAt.lt(cutoff)));
        if has_provider_file {
            update = update
                .col_expr(
                    attachments::Column::CleanupStatus,
                    Expr::value(Some(cleanup::PENDING.to_owned())),
                )
                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)));
        }
        let res = update
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}
