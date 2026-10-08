//! `attachment` repository (chat child: tenant-only scope).

use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Value};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::attachment;

/// Repository for `attachment` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct AttachmentRepo;

impl AttachmentRepo {
    /// Insert a complete row.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (unique/CHECK
    /// violations included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: attachment::Model,
    ) -> Result<attachment::Model, ScopeError> {
        insert_model::<attachment::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// Load a row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<attachment::Model>, ScopeError> {
        attachment::Entity::find_by_id(id)
            .secure()
            .scope_with(&scope.tenant_only())
            .one(runner)
            .await
    }

    /// Chat deletion: set `cleanup_status = 'pending'` and
    /// `cleanup_updated_at = now` on every non-deleted attachment of the chat
    /// that has no cleanup status yet. Returns the number of rows marked.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_chat_cleanup_pending(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = attachment::Entity::update_many()
            .filter(attachment::Column::ChatId.eq(chat_id))
            .filter(attachment::Column::DeletedAt.is_null())
            .filter(attachment::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Ready, non-deleted attachments of the chat.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_ready(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<attachment::Model>, ScopeError> {
        attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .all(runner)
            .await
    }

    /// Attachments of the chat with the given ids (deleted rows included;
    /// unknown ids are absent).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_in_chat(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<attachment::Model>, ScopeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(ids.iter().copied())),
            )
            .all(runner)
            .await
    }

    /// Non-deleted, non-failed attachments of the chat counted against the
    /// per-chat limits: `(documents, total size in bytes)`.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn chat_usage(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<(u64, u64), ScopeError> {
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Status.ne("failed")),
            )
            .all(runner)
            .await?;
        let documents = rows
            .iter()
            .filter(|a| a.attachment_kind == "document")
            .count() as u64;
        let bytes = rows
            .iter()
            .map(|a| u64::try_from(a.size_bytes).unwrap_or(0))
            .sum();
        Ok((documents, bytes))
    }

    /// Conditional update of one attachment of a chat: `sets` are applied
    /// when the row matches `guard`. Returns the number of rows updated.
    async fn update_guarded(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        guard: Condition,
        sets: Vec<(attachment::Column, SimpleExpr)>,
    ) -> Result<u64, ScopeError> {
        let mut q = attachment::Entity::update_many()
            .filter(attachment::Column::ChatId.eq(chat_id))
            .filter(attachment::Column::Id.eq(id))
            .filter(guard)
            .secure()
            .scope_with(&scope.tenant_only());
        for (col, value) in sets {
            q = q.col_expr(col, value);
        }
        Ok(q.exec(runner).await?.rows_affected)
    }

    /// `pending` → `uploaded` with the provider file id.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_uploaded(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all().add(attachment::Column::Status.eq("pending")),
            vec![
                (attachment::Column::Status, Expr::value("uploaded")),
                (
                    attachment::Column::ProviderFileId,
                    Expr::value(provider_file_id.to_owned()),
                ),
                (attachment::Column::UpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// Record the secondary (Anthropic) copy state of a non-deleted row:
    /// `secondary_status`, `secondary_provider_kind = 'anthropic'` and, when
    /// given, `secondary_file_id`.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_secondary(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        status: &str,
        secondary_file_id: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let mut sets = vec![
            (
                attachment::Column::SecondaryStatus,
                Expr::value(status.to_owned()),
            ),
            (
                attachment::Column::SecondaryProviderKind,
                Expr::value("anthropic"),
            ),
            (attachment::Column::UpdatedAt, Expr::value(now)),
        ];
        if let Some(file_id) = secondary_file_id {
            sets.push((
                attachment::Column::SecondaryFileId,
                Expr::value(file_id.to_owned()),
            ));
        }
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all().add(attachment::Column::DeletedAt.is_null()),
            sets,
        )
        .await
    }

    /// `uploaded` → `ready` (with the image thumbnail `(webp, width,
    /// height)`), only while the row is not deleted and not claimed by a
    /// cleanup (`cleanup_status IS NULL`).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_ready(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        thumbnail: Option<(Vec<u8>, i32, i32)>,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let mut sets = vec![
            (attachment::Column::Status, Expr::value("ready")),
            (attachment::Column::UpdatedAt, Expr::value(now)),
        ];
        if let Some((webp, width, height)) = thumbnail {
            sets.push((attachment::Column::ImgThumbnail, Expr::value(webp)));
            sets.push((attachment::Column::ImgThumbnailWidth, Expr::value(width)));
            sets.push((attachment::Column::ImgThumbnailHeight, Expr::value(height)));
        }
        Self::update_guarded(runner, scope, chat_id, id, Self::live_uploaded(), sets).await
    }

    /// `pending` / `uploaded` → `failed` with `error_code` (upload request).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_failed(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all().add(attachment::Column::Status.is_in(["pending", "uploaded"])),
            vec![
                (attachment::Column::Status, Expr::value("failed")),
                (
                    attachment::Column::ErrorCode,
                    Expr::value(error_code.to_owned()),
                ),
                (attachment::Column::UpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// Background indexing failure: a live `uploaded` row becomes `failed`
    /// (`indexing_failed`) with `cleanup_status = 'pending'`.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn fail_indexing_for_cleanup(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Self::live_uploaded(),
            vec![
                (attachment::Column::Status, Expr::value("failed")),
                (
                    attachment::Column::ErrorCode,
                    Expr::value("indexing_failed"),
                ),
                (attachment::Column::CleanupStatus, Expr::value("pending")),
                (attachment::Column::CleanupUpdatedAt, Expr::value(now)),
                (attachment::Column::UpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// Background indexing heartbeat: refresh `updated_at` of a live
    /// `uploaded` row (0 rows: deleted, finished or claimed by a cleanup).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn touch_uploaded(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Self::live_uploaded(),
            vec![(attachment::Column::UpdatedAt, Expr::value(now))],
        )
        .await
    }

    /// Soft-delete a non-deleted attachment and mark its provider cleanup
    /// pending. Returns the number of rows deleted (0: already deleted).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn soft_delete(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all().add(attachment::Column::DeletedAt.is_null()),
            vec![
                (attachment::Column::DeletedAt, Expr::value(now)),
                (attachment::Column::UpdatedAt, Expr::value(now)),
                (attachment::Column::CleanupStatus, Expr::value("pending")),
                (attachment::Column::CleanupUpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// Attachments of the chat (deleted rows included) whose
    /// `cleanup_status` is `status`, oldest first.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_by_cleanup_status(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        status: &str,
    ) -> Result<Vec<attachment::Model>, ScopeError> {
        attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::CleanupStatus.eq(status)),
            )
            .order_by(attachment::Column::CreatedAt, Order::Asc)
            .order_by(attachment::Column::Id, Order::Asc)
            .all(runner)
            .await
    }

    /// Provider cleanup finished: `cleanup_status` `pending` → `done`.
    /// Returns the number of rows updated (0: no longer `pending`).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_cleanup_done(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all().add(attachment::Column::CleanupStatus.eq("pending")),
            vec![
                (attachment::Column::CleanupStatus, Expr::value("done")),
                (attachment::Column::CleanupUpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// One failed provider delete of a `pending` row whose
    /// `cleanup_attempts` is still `attempts`: set it to `attempts + 1` with
    /// `last_cleanup_error` and `cleanup_updated_at`; `terminal` also moves
    /// the row to `cleanup_status = 'failed'`. Returns the number of rows
    /// updated (0: the row changed concurrently).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_cleanup_failure(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        attempts: i32,
        error: &str,
        terminal: bool,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let status = if terminal { "failed" } else { "pending" };
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all()
                .add(attachment::Column::CleanupStatus.eq("pending"))
                .add(attachment::Column::CleanupAttempts.eq(attempts)),
            vec![
                (attachment::Column::CleanupStatus, Expr::value(status)),
                (
                    attachment::Column::CleanupAttempts,
                    Expr::value(attempts.saturating_add(1)),
                ),
                (
                    attachment::Column::LastCleanupError,
                    Expr::value(error.to_owned()),
                ),
                (attachment::Column::CleanupUpdatedAt, Expr::value(now)),
            ],
        )
        .await
    }

    /// At most `limit` abandoned uploads within `scope` (the reaper uses an
    /// unconstrained scope: every tenant): `status IN ('pending',
    /// 'uploaded') AND deleted_at IS NULL AND cleanup_status IS NULL AND
    /// updated_at < :cutoff`, oldest `updated_at` first. `cutoff` must be
    /// bound in the column's comparable form (`timestamps::comparable`).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_abandoned(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        cutoff: &Value,
        limit: u64,
    ) -> Result<Vec<attachment::Model>, ScopeError> {
        attachment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff.clone())),
            )
            .order_by(attachment::Column::UpdatedAt, Order::Asc)
            .order_by(attachment::Column::Id, Order::Asc)
            .limit(limit)
            .all(runner)
            .await
    }

    /// Reaper CAS: `failed` / `upload_abandoned` when the row is still
    /// `from_status`, not deleted, without `cleanup_status` and
    /// `updated_at < :cutoff`; `with_cleanup` also sets `cleanup_status =
    /// 'pending'`. Returns the number of rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    #[allow(clippy::too_many_arguments)]
    pub async fn mark_abandoned(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        from_status: &str,
        cutoff: &Value,
        with_cleanup: bool,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let mut sets = vec![
            (attachment::Column::Status, Expr::value("failed")),
            (
                attachment::Column::ErrorCode,
                Expr::value("upload_abandoned"),
            ),
            (attachment::Column::UpdatedAt, Expr::value(now)),
        ];
        if with_cleanup {
            sets.push((attachment::Column::CleanupStatus, Expr::value("pending")));
            sets.push((attachment::Column::CleanupUpdatedAt, Expr::value(now)));
        }
        Self::update_guarded(
            runner,
            scope,
            chat_id,
            id,
            Condition::all()
                .add(attachment::Column::Status.eq(from_status))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::UpdatedAt.lt(cutoff.clone())),
            sets,
        )
        .await
    }

    /// `status = 'uploaded' AND deleted_at IS NULL AND cleanup_status IS NULL`.
    fn live_uploaded() -> Condition {
        Condition::all()
            .add(attachment::Column::Status.eq("uploaded"))
            .add(attachment::Column::DeletedAt.is_null())
            .add(attachment::Column::CleanupStatus.is_null())
    }
}
