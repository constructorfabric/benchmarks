//! `AttachmentRepository`, `AttachmentLifecycleRepository` and
//! `AttachmentCleanupRepository` over the Secure ORM (tenant scope derived from
//! the owner-scoped chat lookup, or the tenant of a cleanup message).
//!
//! Every status change is a conditional `UPDATE ... WHERE <expected state>`:
//! the request, the background indexing task, the reaper and the cleanup
//! handlers race on the same rows, and `rows_affected` tells the winner.

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::attachments::{
    AttachmentRow, AttachmentUsage, NewAttachment, NewThumbnail, cleanup_status, error_code, status,
};
use crate::domain::error::DomainError;
use crate::domain::messages::ReadyAttachment;
use crate::domain::ports::{
    AttachmentCleanupRepository, AttachmentLifecycleRepository, AttachmentRepository,
    NewMessageAttachment,
};
use crate::infra::db::entity::{attachments, message_attachments};
use crate::infra::db::timestamp::{from_db, to_db};

const KIND_DOCUMENT: &str = "document";

fn to_ready(model: attachments::Model) -> ReadyAttachment {
    ReadyAttachment {
        id: model.id,
        filename: model.filename,
        kind: model.attachment_kind,
        provider_file_id: model.provider_file_id,
    }
}

fn to_row(model: attachments::Model) -> AttachmentRow {
    AttachmentRow {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        uploaded_by_user_id: model.uploaded_by_user_id,
        filename: model.filename,
        content_type: model.content_type,
        size_bytes: model.size_bytes,
        storage_backend: model.storage_backend,
        provider_file_id: model.provider_file_id,
        status: model.status,
        error_code: model.error_code,
        kind: model.attachment_kind,
        for_file_search: model.for_file_search,
        for_code_interpreter: model.for_code_interpreter,
        thumbnail: model.img_thumbnail,
        thumbnail_width: model.img_thumbnail_width,
        thumbnail_height: model.img_thumbnail_height,
        secondary_file_id: model.secondary_file_id,
        cleanup_status: model.cleanup_status,
        created_at: from_db(model.created_at),
        updated_at: from_db(model.updated_at),
        deleted_at: model.deleted_at.map(from_db),
    }
}

#[async_trait]
impl<R: DBRunner> AttachmentRepository for R {
    async fn find_ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ReadyAttachment>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let models = attachments::Entity::find()
            .filter(attachments::Column::Id.is_in(ids.iter().copied()))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::UploadedByUserId.eq(user_id))
            .filter(attachments::Column::Status.eq(status::READY))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_ready).collect())
    }

    async fn ready_documents(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyAttachment>, DomainError> {
        let models = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::AttachmentKind.eq(KIND_DOCUMENT))
            .filter(attachments::Column::ForFileSearch.eq(true))
            .filter(attachments::Column::Status.eq(status::READY))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_ready).collect())
    }

    async fn ready_code_interpreter_files(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyAttachment>, DomainError> {
        let models = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::ForCodeInterpreter.eq(true))
            .filter(attachments::Column::Status.eq(status::READY))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .order_by(attachments::Column::CreatedAt, Order::Asc)
            .order_by(attachments::Column::Id, Order::Asc)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_ready).collect())
    }

    async fn link_attachments(
        &self,
        scope: &AccessScope,
        links: &[NewMessageAttachment],
    ) -> Result<(), DomainError> {
        for link in links {
            secure_insert::<message_attachments::Entity>(
                message_attachments::ActiveModel {
                    tenant_id: Set(link.tenant_id),
                    chat_id: Set(link.chat_id),
                    message_id: Set(link.message_id),
                    attachment_id: Set(link.attachment_id),
                    created_at: Set(to_db(link.now)),
                },
                scope,
                self,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<R: DBRunner> AttachmentLifecycleRepository for R {
    async fn attachment_usage(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<AttachmentUsage, DomainError> {
        let models = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::Status.ne(status::FAILED))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .all(self)
            .await?;
        let mut usage = AttachmentUsage::default();
        for model in models {
            if model.attachment_kind == KIND_DOCUMENT {
                usage.documents += 1;
            }
            usage.total_bytes = usage
                .total_bytes
                .saturating_add(u64::try_from(model.size_bytes.unwrap_or_default()).unwrap_or(0));
        }
        Ok(usage)
    }

    async fn insert_attachment(
        &self,
        scope: &AccessScope,
        attachment: NewAttachment,
    ) -> Result<(), DomainError> {
        secure_insert::<attachments::Entity>(
            attachments::ActiveModel {
                id: Set(attachment.id),
                tenant_id: Set(attachment.tenant_id),
                chat_id: Set(attachment.chat_id),
                uploaded_by_user_id: Set(attachment.user_id),
                filename: Set(attachment.filename),
                content_type: Set(Some(attachment.content_type)),
                size_bytes: Set(Some(attachment.size_bytes)),
                storage_backend: Set(attachment.storage_backend),
                provider_file_id: Set(None),
                status: Set(status::PENDING.to_owned()),
                error_code: Set(None),
                attachment_kind: Set(attachment.kind.as_str().to_owned()),
                for_file_search: Set(attachment.for_file_search),
                for_code_interpreter: Set(attachment.for_code_interpreter),
                doc_summary: Set(None),
                img_thumbnail: Set(None),
                img_thumbnail_width: Set(None),
                img_thumbnail_height: Set(None),
                summary_model: Set(None),
                summary_updated_at: Set(None),
                cleanup_status: Set(None),
                cleanup_attempts: Set(0),
                last_cleanup_error: Set(None),
                cleanup_updated_at: Set(None),
                created_at: Set(to_db(attachment.now)),
                updated_at: Set(to_db(attachment.now)),
                deleted_at: Set(None),
                secondary_file_id: Set(None),
                secondary_status: Set("not_attempted".to_owned()),
                secondary_provider_kind: Set(None),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn find_attachment(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<AttachmentRow>, DomainError> {
        let model = attachments::Entity::find()
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn mark_attachment_uploaded(
        &self,
        scope: &AccessScope,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(status::UPLOADED))
            .col_expr(
                attachments::Column::ProviderFileId,
                Expr::value(provider_file_id),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::Status.eq(status::PENDING))
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn mark_attachment_ready(
        &self,
        scope: &AccessScope,
        id: Uuid,
        thumbnail: Option<&NewThumbnail>,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let mut update = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(status::READY))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)));
        if let Some(thumbnail) = thumbnail {
            update = update
                .col_expr(
                    attachments::Column::ImgThumbnail,
                    Expr::value(thumbnail.bytes.clone()),
                )
                .col_expr(
                    attachments::Column::ImgThumbnailWidth,
                    Expr::value(thumbnail.width),
                )
                .col_expr(
                    attachments::Column::ImgThumbnailHeight,
                    Expr::value(thumbnail.height),
                );
        }
        let updated = update
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::Status.eq(status::UPLOADED))
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn mark_attachment_failed(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(status::FAILED))
            .col_expr(attachments::Column::ErrorCode, Expr::value(error))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::Status.is_in([status::PENDING, status::UPLOADED]))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn refresh_uploaded_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::Status.eq(status::UPLOADED))
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn fail_indexing_with_cleanup(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(status::FAILED))
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(error_code::INDEXING_FAILED),
            )
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(cleanup_status::PENDING),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::Status.eq(status::UPLOADED))
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn soft_delete_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::DeletedAt, Expr::value(to_db(now)))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn mark_attachment_cleanup_pending(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(cleanup_status::PENDING),
            )
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn is_attachment_linked(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let link = message_attachments::Entity::find()
            .filter(message_attachments::Column::ChatId.eq(chat_id))
            .filter(message_attachments::Column::AttachmentId.eq(id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(link.is_some())
    }
}

#[async_trait]
impl<R: DBRunner> AttachmentCleanupRepository for R {
    async fn stale_attachments(
        &self,
        scope: &AccessScope,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<AttachmentRow>, DomainError> {
        let models = attachments::Entity::find()
            .filter(attachments::Column::Status.is_in([status::PENDING, status::UPLOADED]))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::UpdatedAt.lt(to_db(cutoff)))
            .secure()
            .scope_with(scope)
            .order_by(attachments::Column::UpdatedAt, Order::Asc)
            .order_by(attachments::Column::Id, Order::Asc)
            .limit(limit)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_row).collect())
    }

    async fn reap_attachment(
        &self,
        scope: &AccessScope,
        candidate: &AttachmentRow,
        cutoff: OffsetDateTime,
        cleanup: bool,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let mut update = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(status::FAILED))
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(error_code::UPLOAD_ABANDONED),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(to_db(now)));
        if cleanup {
            update = update.col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(cleanup_status::PENDING),
            );
        }
        let updated = update
            .filter(attachments::Column::Id.eq(candidate.id))
            .filter(attachments::Column::Status.eq(candidate.status.as_str()))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::UpdatedAt.lt(to_db(cutoff)))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn pending_cleanup_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<AttachmentRow>, DomainError> {
        let models = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::CleanupStatus.eq(cleanup_status::PENDING))
            .secure()
            .scope_with(scope)
            .order_by(attachments::Column::CreatedAt, Order::Asc)
            .order_by(attachments::Column::Id, Order::Asc)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_row).collect())
    }

    async fn mark_cleanup_done(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(cleanup_status::DONE),
            )
            .col_expr(
                attachments::Column::CleanupUpdatedAt,
                Expr::value(to_db(now)),
            )
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::CleanupStatus.eq(cleanup_status::PENDING))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn record_cleanup_attempt(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupAttempts,
                Expr::col(attachments::Column::CleanupAttempts).add(1),
            )
            .col_expr(attachments::Column::LastCleanupError, Expr::value(error))
            .col_expr(
                attachments::Column::CleanupUpdatedAt,
                Expr::value(to_db(now)),
            )
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::CleanupStatus.eq(cleanup_status::PENDING))
            .secure()
            .scope_with(scope)
            .exec_with_returning(self)
            .await?;
        Ok(updated
            .into_iter()
            .next()
            .map(|model| model.cleanup_attempts))
    }

    async fn mark_cleanup_failed(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(cleanup_status::FAILED),
            )
            .col_expr(
                attachments::Column::CleanupUpdatedAt,
                Expr::value(to_db(now)),
            )
            .filter(attachments::Column::Id.eq(id))
            .filter(attachments::Column::CleanupStatus.eq(cleanup_status::PENDING))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }
}
