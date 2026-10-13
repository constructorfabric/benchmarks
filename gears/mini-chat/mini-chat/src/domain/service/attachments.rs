//! Attachments: upload/get/delete, vector store protocol, background indexing (OWNER: attachments).
//!
//! Upload flow (DESIGN "File Upload"): the handler calls [`AttachmentService::prepare_upload`]
//! (chat + model resolution, before the body is read), validates the `file` part header with
//! [`AttachmentService::validate_file`], streams the body under the size limit and then calls
//! [`AttachmentService::upload`].

mod indexing;
pub mod mime;
#[cfg(test)]
pub(crate) mod test_helpers;
mod vector_store;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::outbox_payloads::{AttachmentCleanupEvent, attachment_event_types};
use crate::domain::service::Deps;
use crate::domain::service::chat_access::load_chat;
use crate::domain::service::thumbnail;
use crate::infra::db::entity::{attachment, chat, message_attachment};
use crate::infra::llm::VectorFileStatus;

pub use mime::AttachmentKind;

/// `attachments.status` values.
pub mod status {
    pub const PENDING: &str = "pending";
    pub const UPLOADED: &str = "uploaded";
    pub const READY: &str = "ready";
    pub const FAILED: &str = "failed";
}

/// `attachments.error_code` values.
pub mod error_codes {
    pub const UPLOAD_FAILED: &str = "upload_failed";
    pub const VECTOR_STORE_FAILED: &str = "vector_store_failed";
    pub const INDEXING_FAILED: &str = "indexing_failed";
    pub const UPLOAD_ABANDONED: &str = "upload_abandoned";
}

/// `attachments.cleanup_status` values.
pub mod cleanup_status {
    pub const PENDING: &str = "pending";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// Generic 503 detail of storage failures.
pub const STORAGE_UNAVAILABLE_DETAIL: &str = "Service temporarily unavailable";

/// Background indexing heartbeat (seconds).
pub const BACKGROUND_ROUND_SECS: u64 = 20;
/// Minimum `upload_reaper.stale_after_secs`.
pub const MIN_STALE_AFTER_SECS: u64 = 60;
// The heartbeat must be at most half the minimum reaper staleness (B.9.5).
const _: () = assert!(BACKGROUND_ROUND_SECS * 2 <= MIN_STALE_AFTER_SECS);

/// Poll intervals and deadlines of the upload flow (DESIGN values by default; tests shorten them).
#[derive(Debug, Clone)]
pub struct UploadTimings {
    /// Indexing wait deadline, measured from the start of the upload request (25 s).
    pub request_deadline: Duration,
    /// First / maximum wait between status reads within the request (250 ms → 2 s).
    pub sync_poll_initial: Duration,
    pub sync_poll_max: Duration,
    /// Background task: heartbeat round (20 s) and total wait (10 min).
    pub background_round: Duration,
    pub background_total: Duration,
    /// Background task: first / maximum wait between status reads (250 ms → 5 s).
    pub background_poll_initial: Duration,
    pub background_poll_max: Duration,
    /// Waits before the retries of a failed `ready` write (1, 2, 4 s).
    pub set_ready_retry: Vec<Duration>,
    /// Vector store creation loser: polls (5) and first wait (doubling).
    pub vs_loser_polls: u32,
    pub vs_loser_initial: Duration,
    /// Age after which a NULL placeholder is reclaimed (120 s).
    pub vs_stale_placeholder: Duration,
}

impl Default for UploadTimings {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(25),
            sync_poll_initial: Duration::from_millis(250),
            sync_poll_max: Duration::from_secs(2),
            background_round: Duration::from_secs(BACKGROUND_ROUND_SECS),
            background_total: Duration::from_secs(600),
            background_poll_initial: Duration::from_millis(250),
            background_poll_max: Duration::from_secs(5),
            set_ready_retry: vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            vs_loser_polls: 5,
            vs_loser_initial: Duration::from_millis(200),
            vs_stale_placeholder: Duration::from_secs(120),
        }
    }
}

/// The chat an upload goes to, resolved before the body is read.
#[derive(Debug, Clone)]
pub struct UploadTarget {
    pub chat: chat::Model,
    /// Tenant-only scope for child tables (always combined with `chat_id`).
    pub child_scope: AccessScope,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
    /// Storage-capable provider entry (`rag_provider` or the model's provider).
    pub storage_provider_id: String,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub storage_backend: String,
}

/// A validated `file` part (header only; the body is read afterwards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub filename: String,
    /// Canonical content type stored on the row.
    pub content_type: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Effective per-file limit in bytes.
    pub max_bytes: u64,
}

pub struct AttachmentService {
    deps: Arc<Deps>,
    timings: UploadTimings,
}

pub(crate) fn storage_unavailable() -> DomainError {
    DomainError::ServiceUnavailable {
        retry_after_secs: 10,
        detail: STORAGE_UNAVAILABLE_DETAIL.to_owned(),
    }
}

pub(crate) const fn attachment_not_found() -> DomainError {
    DomainError::NotFound {
        resource: resource_types::ATTACHMENT,
    }
}

impl AttachmentService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self {
            deps,
            timings: UploadTimings::default(),
        }
    }

    /// Replaces the poll intervals / deadlines (tests).
    #[must_use]
    pub fn with_timings(mut self, timings: UploadTimings) -> Self {
        self.timings = timings;
        self
    }

    #[must_use]
    pub const fn timings(&self) -> &UploadTimings {
        &self.timings
    }

    /// Loads the chat (`upload_attachment`) and resolves its model and storage provider.
    ///
    /// # Errors
    /// 403/503 (PEP), 404 chat, 400 `INVALID_MODEL`, 500 on plugin / provider resolution failure.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadTarget, DomainError> {
        let authorized = load_chat(&self.deps, ctx, chat_id, actions::UPLOAD_ATTACHMENT).await?;
        let (snapshot, model) = self
            .deps
            .policy
            .resolve_model(ctx.subject_id(), &authorized.chat.model, false)
            .await?;
        let storage_provider_id = self
            .deps
            .providers
            .storage_provider_id(&model.provider_id)
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "no provider entry '{}' for model '{}'",
                    model.provider_id, model.id
                ))
            })?;
        let resolved = self
            .deps
            .providers
            .resolve(&storage_provider_id, authorized.chat.tenant_id)
            .ok_or_else(|| {
                DomainError::internal(format!("no storage provider entry '{storage_provider_id}'"))
            })?;
        Ok(UploadTarget {
            chat: authorized.chat,
            child_scope: authorized.child_scope,
            model,
            kill_switches: snapshot.kill_switches,
            storage_provider_id,
            storage_backend: resolved.storage_backend,
        })
    }

    /// Validates the `file` part header: MIME allow-list (with extension inference for
    /// `application/octet-stream`), kill switches, code-interpreter availability, and computes
    /// the effective size limit.
    ///
    /// # Errors
    /// 400 `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED` (images), `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn validate_file(
        &self,
        target: &UploadTarget,
        part_content_type: &str,
        filename: Option<&str>,
    ) -> Result<FileSpec, DomainError> {
        let rag = &self.deps.cfg.rag;
        let filename = mime::normalize_filename(filename);
        let effective = mime::effective_content_type(part_content_type, &filename);
        let mime::MimeCheck::Supported(content_type) = mime::check(&effective, rag.allow_csv_upload)
        else {
            return Err(DomainError::invalid(
                resource_types::ATTACHMENT,
                "content_type",
                reasons::UNSUPPORTED_CONTENT_TYPE,
                format!("Unsupported content type: {effective}"),
            ));
        };
        let kind = mime::kind_of(content_type);
        let (for_file_search, mut for_code_interpreter) = mime::purposes_of(content_type);
        if kind == AttachmentKind::Image && target.kill_switches.disable_images {
            return Err(DomainError::feature_disabled("images"));
        }
        let ci_available = !target.kill_switches.disable_code_interpreter
            && target.model.general_config.tool_support.code_interpreter;
        if for_code_interpreter && !ci_available {
            if !for_file_search {
                return Err(DomainError::invalid(
                    resource_types::ATTACHMENT,
                    "file",
                    reasons::CODE_INTERPRETER_UNAVAILABLE,
                    "Code interpreter is not available for this chat",
                ));
            }
            for_code_interpreter = false;
        }
        let cfg_kb = match kind {
            AttachmentKind::Image => rag.uploaded_image_max_size_kb,
            AttachmentKind::Document => rag.uploaded_file_max_size_kb,
        };
        let max_bytes = (u64::from(cfg_kb) * 1024)
            .min(u64::from(target.model.general_config.max_file_size_mb) * 1024 * 1024);
        Ok(FileSpec {
            filename,
            content_type: content_type.to_owned(),
            kind,
            for_file_search,
            for_code_interpreter,
            max_bytes,
        })
    }

    /// Stores a validated file: per-chat limits, row insert, provider upload, then the
    /// kind-specific path (thumbnail / code interpreter / vector store indexing).
    /// `started` is the start of the upload request (the indexing deadline is measured from it).
    ///
    /// # Errors
    /// 409 `provider_mismatch`, 429 `document_limit` / `storage_limit`, 503 storage failures,
    /// 500 internal.
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        target: &UploadTarget,
        spec: FileSpec,
        data: bytes::Bytes,
        started: Instant,
    ) -> Result<attachment::Model, DomainError> {
        let size = i64::try_from(data.len()).map_err(DomainError::internal)?;
        if u64::try_from(size).unwrap_or(u64::MAX) > spec.max_bytes {
            return Err(file_too_large(spec.max_bytes));
        }
        let chat = &target.chat;
        let scope = &target.child_scope;
        {
            let conn = self.deps.db.conn()?;
            self.check_chat_limits(&conn, scope, chat.id, spec.kind, size).await?;
            if spec.for_file_search {
                vector_store::check_provider(&conn, scope, chat.id, &target.storage_backend).await?;
            }
        }

        let now = OffsetDateTime::now_utc();
        let id = Uuid::new_v4();
        let am = attachment::ActiveModel {
            id: Set(id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            uploaded_by_user_id: Set(ctx.subject_id()),
            filename: Set(spec.filename.clone()),
            content_type: Set(spec.content_type.clone()),
            size_bytes: Set(size),
            storage_backend: Set(target.storage_backend.clone()),
            provider_file_id: Set(None),
            status: Set(status::PENDING.to_owned()),
            error_code: Set(None),
            attachment_kind: Set(spec.kind.as_str().to_owned()),
            for_file_search: Set(spec.for_file_search),
            for_code_interpreter: Set(spec.for_code_interpreter),
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
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
            secondary_file_id: Set(None),
            secondary_status: Set("not_attempted".to_owned()),
            secondary_provider_kind: Set(None),
        };
        {
            let conn = self.deps.db.conn()?;
            secure_insert::<attachment::Entity>(am, scope, &conn).await?;
        }

        // Provider upload (not retried, no idempotency key).
        let provider_filename = format!(
            "{}_{}.{}",
            chat.id,
            id,
            mime::provider_extension(&spec.content_type)
        );
        let file_id = match self
            .deps
            .storage
            .upload_file(
                &target.storage_provider_id,
                chat.tenant_id,
                &provider_filename,
                &spec.content_type,
                data.clone(),
            )
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(attachment_id = %id, error = %e, "provider file upload failed");
                self.mark_failed(scope, chat.id, id, error_codes::UPLOAD_FAILED).await;
                return Err(storage_unavailable());
            }
        };
        let updated = self
            .update_row(
                scope,
                chat.id,
                id,
                vec![
                    (attachment::Column::Status, Expr::value(status::UPLOADED)),
                    (attachment::Column::ProviderFileId, Expr::value(file_id.clone())),
                ],
            )
            .await?;
        if updated == 0 {
            // Deleted (directly or with its chat) during the provider upload: the cleanup
            // event carries no provider file id, so delete the file here.
            tracing::info!(attachment_id = %id, "attachment deleted during upload; deleting provider file");
            self.delete_provider_file_best_effort(&target.storage_provider_id, chat.tenant_id, &file_id);
            return self.load_row(scope, chat.id, id).await;
        }

        match spec.kind {
            AttachmentKind::Image => {
                let cfg = self.deps.cfg.thumbnail.clone();
                let thumb = tokio::task::spawn_blocking(move || thumbnail::generate(&data, &cfg))
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "thumbnail task failed");
                        None
                    });
                let mut cols = vec![(attachment::Column::Status, Expr::value(status::READY))];
                if let Some(t) = thumb {
                    cols.push((attachment::Column::ImgThumbnail, Expr::value(t.data)));
                    cols.push((
                        attachment::Column::ImgThumbnailWidth,
                        Expr::value(i32::try_from(t.width).unwrap_or(i32::MAX)),
                    ));
                    cols.push((
                        attachment::Column::ImgThumbnailHeight,
                        Expr::value(i32::try_from(t.height).unwrap_or(i32::MAX)),
                    ));
                }
                self.update_row(scope, chat.id, id, cols).await?;
            }
            AttachmentKind::Document if !spec.for_file_search => {
                self.update_row(
                    scope,
                    chat.id,
                    id,
                    vec![(attachment::Column::Status, Expr::value(status::READY))],
                )
                .await?;
            }
            AttachmentKind::Document => {
                self.index_document(target, id, &file_id, started).await?;
            }
        }
        self.load_row(scope, chat.id, id).await
    }

    /// Vector store path of a `file_search` document.
    async fn index_document(
        &self,
        target: &UploadTarget,
        id: Uuid,
        file_id: &str,
        started: Instant,
    ) -> Result<(), DomainError> {
        let chat = &target.chat;
        let scope = &target.child_scope;
        let vs_id = match vector_store::ensure(
            &self.deps,
            &self.timings,
            scope,
            chat,
            &target.storage_provider_id,
            &target.storage_backend,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(attachment_id = %id, error = %e, "chat vector store unavailable");
                self.mark_failed(scope, chat.id, id, error_codes::VECTOR_STORE_FAILED)
                    .await;
                self.delete_provider_file_best_effort(&target.storage_provider_id, chat.tenant_id, file_id);
                return Err(e);
            }
        };
        let mut attrs = BTreeMap::new();
        attrs.insert("attachment_id".to_owned(), id.to_string());
        let added = self
            .deps
            .storage
            .add_file_to_vector_store(
                &target.storage_provider_id,
                chat.tenant_id,
                &vs_id,
                file_id,
                attrs,
            )
            .await;
        let initial = match added {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(attachment_id = %id, error = %e, "add file to vector store failed");
                return Err(self.indexing_failed(target, id, file_id).await);
            }
        };
        let deadline = started + self.timings.request_deadline;
        let outcome = indexing::wait_in_request(
            &self.deps,
            &self.timings,
            &target.storage_provider_id,
            chat.tenant_id,
            &vs_id,
            file_id,
            initial,
            deadline,
        )
        .await;
        match outcome {
            indexing::SyncOutcome::Ready => {
                self.update_row(
                    scope,
                    chat.id,
                    id,
                    vec![(attachment::Column::Status, Expr::value(status::READY))],
                )
                .await
                .map(|_| ())
            }
            indexing::SyncOutcome::Failed(reason) => {
                tracing::warn!(attachment_id = %id, reason = %reason, "document indexing failed");
                Err(self.indexing_failed(target, id, file_id).await)
            }
            indexing::SyncOutcome::StillIndexing => {
                indexing::spawn_background(
                    Arc::clone(&self.deps),
                    self.timings.clone(),
                    indexing::BackgroundJob {
                        tenant_id: chat.tenant_id,
                        chat_id: chat.id,
                        attachment_id: id,
                        storage_provider_id: target.storage_provider_id.clone(),
                        storage_backend: target.storage_backend.clone(),
                        vector_store_id: vs_id,
                        provider_file_id: file_id.to_owned(),
                    },
                );
                Ok(())
            }
        }
    }

    async fn indexing_failed(&self, target: &UploadTarget, id: Uuid, file_id: &str) -> DomainError {
        self.mark_failed(&target.child_scope, target.chat.id, id, error_codes::INDEXING_FAILED)
            .await;
        self.delete_provider_file_best_effort(&target.storage_provider_id, target.chat.tenant_id, file_id);
        storage_unavailable()
    }

    /// Fire-and-forget provider file delete (not retried).
    fn delete_provider_file_best_effort(&self, provider_id: &str, tenant_id: Uuid, file_id: &str) {
        let storage = Arc::clone(&self.deps.storage);
        let (provider_id, file_id) = (provider_id.to_owned(), file_id.to_owned());
        self.deps.tasks.spawn(async move {
            if let Err(e) = storage.delete_file(&provider_id, tenant_id, &file_id).await
                && !e.is_not_found()
            {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    /// Per-chat limits on non-deleted, non-failed attachments.
    async fn check_chat_limits(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        kind: AttachmentKind,
        new_size: i64,
    ) -> Result<(), DomainError> {
        let rag = &self.deps.cfg.rag;
        let rows: Vec<(String, i64)> = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Status.ne(status::FAILED)),
            )
            .secure()
            .scope_with(scope)
            .project_all(runner, |q| {
                use sea_orm::QuerySelect;
                q.select_only()
                    .column(attachment::Column::AttachmentKind)
                    .column(attachment::Column::SizeBytes)
                    .into_model::<KindSize>()
            })
            .await?
            .into_iter()
            .map(|r| (r.attachment_kind, r.size_bytes))
            .collect();
        if kind == AttachmentKind::Document {
            let docs = rows
                .iter()
                .filter(|(k, _)| k == AttachmentKind::Document.as_str())
                .count();
            if docs >= usize::try_from(rag.max_documents_per_chat).unwrap_or(usize::MAX) {
                return Err(DomainError::ResourceExhausted {
                    resource: resource_types::ATTACHMENT,
                    subject: reasons::DOCUMENT_LIMIT.to_owned(),
                    description: format!(
                        "Per-chat document limit ({}) reached",
                        rag.max_documents_per_chat
                    ),
                    detail: "Per-chat document limit reached".to_owned(),
                });
            }
        }
        let total: i64 = rows.iter().map(|(_, s)| *s).sum();
        let limit = i64::from(rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        if total.saturating_add(new_size) > limit {
            return Err(DomainError::ResourceExhausted {
                resource: resource_types::ATTACHMENT,
                subject: reasons::STORAGE_LIMIT.to_owned(),
                description: format!(
                    "Per-chat upload size limit ({} MB) reached",
                    rag.max_total_upload_mb_per_chat
                ),
                detail: "Per-chat storage limit reached".to_owned(),
            });
        }
        Ok(())
    }

    /// Updates columns of one attachment row (plus `updated_at`). The update is guarded by
    /// `deleted_at IS NULL AND cleanup_status IS NULL`: a row deleted directly or through its
    /// chat meanwhile is left alone (cleanup owns it). Returns the affected row count.
    async fn update_row(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        cols: Vec<(attachment::Column, sea_orm::sea_query::SimpleExpr)>,
    ) -> Result<u64, DomainError> {
        let conn = self.deps.db.conn()?;
        let mut q = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(OffsetDateTime::now_utc()));
        for (c, v) in cols {
            q = q.col_expr(c, v);
        }
        let res = q
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null()),
            )
            .secure()
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(res.rows_affected)
    }

    /// Marks the row `failed` with `error_code` (errors are logged; the caller reports its own).
    async fn mark_failed(&self, scope: &AccessScope, chat_id: Uuid, id: Uuid, code: &str) {
        if let Err(e) = self
            .update_row(
                scope,
                chat_id,
                id,
                vec![
                    (attachment::Column::Status, Expr::value(status::FAILED)),
                    (attachment::Column::ErrorCode, Expr::value(code)),
                ],
            )
            .await
        {
            tracing::error!(attachment_id = %id, error = %e, "failed to mark attachment failed");
        }
    }

    async fn load_row(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<attachment::Model, DomainError> {
        let conn = self.deps.db.conn()?;
        attachment::Entity::find()
            .filter(attachment::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .and_id(id)?
            .one(&conn)
            .await?
            .ok_or_else(attachment_not_found)
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`: a non-deleted attachment of the chat
    /// uploaded by the caller.
    ///
    /// # Errors
    /// 403/503 (PEP), 404 chat / attachment.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<attachment::Model, DomainError> {
        let authorized = load_chat(&self.deps, ctx, chat_id, actions::READ_ATTACHMENT).await?;
        let row = self
            .load_row(&authorized.child_scope, chat_id, attachment_id)
            .await?;
        if row.deleted_at.is_some() || row.uploaded_by_user_id != ctx.subject_id() {
            return Err(attachment_not_found());
        }
        Ok(row)
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`: soft-delete + attachment cleanup
    /// outbox event in one transaction. Idempotent for already-deleted rows.
    ///
    /// # Errors
    /// 403/503 (PEP), 404 chat / attachment, 409 `attachment_locked`, 500 internal.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let authorized = load_chat(&self.deps, ctx, chat_id, actions::DELETE_ATTACHMENT).await?;
        let scope = authorized.child_scope;
        let row = self.load_row(&scope, chat_id, attachment_id).await?;
        if row.uploaded_by_user_id != ctx.subject_id() {
            return Err(attachment_not_found());
        }
        if row.deleted_at.is_some() {
            return Ok(());
        }
        {
            let conn = self.deps.db.conn()?;
            let refs = message_attachment::Entity::find()
                .filter(
                    Condition::all()
                        .add(message_attachment::Column::ChatId.eq(chat_id))
                        .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
                )
                .secure()
                .scope_with(&scope)
                .count(&conn)
                .await?;
            if refs > 0 {
                return Err(DomainError::AlreadyExists {
                    resource: resource_types::ATTACHMENT,
                    name: reasons::ATTACHMENT_LOCKED.to_owned(),
                    detail: "Attachment is referenced by a submitted message".to_owned(),
                });
            }
        }

        let outbox = Arc::clone(&self.deps.outbox);
        let wake = self
            .deps
            .db
            .transaction(move |tx| {
                let scope = scope.clone();
                let outbox = Arc::clone(&outbox);
                let row = row.clone();
                Box::pin(async move {
                    let now = OffsetDateTime::now_utc();
                    let affected = attachment::Entity::update_many()
                        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(cleanup_status::PENDING),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(row.id))
                                .add(attachment::Column::ChatId.eq(row.chat_id))
                                .add(attachment::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if affected == 0 {
                        return Ok(None);
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: attachment_event_types::DELETED.to_owned(),
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: row.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.attachment_kind.clone(),
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    let wake = outbox
                        .enqueue_attachment_cleanup(tx, &ev)
                        .await
                        .map_err(|e| match e {
                            // An oversized payload is a 500 on attachment DELETE.
                            DomainError::InvalidFormat { message, .. } => DomainError::internal(message),
                            other => other,
                        })?;
                    Ok(Some(wake))
                })
            })
            .await?;
        if let Some(w) = wake {
            w.fire();
        }
        Ok(())
    }
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct KindSize {
    attachment_kind: String,
    size_bytes: i64,
}

/// 400 `out_of_range` `FILE_TOO_LARGE`.
#[must_use]
pub fn file_too_large(max_bytes: u64) -> DomainError {
    DomainError::out_of_range(
        resource_types::ATTACHMENT,
        "content_length",
        reasons::FILE_TOO_LARGE,
        format!("File exceeds the maximum size of {max_bytes} bytes"),
    )
}

/// Maps a vector file status to a short log reason.
pub(crate) fn status_reason(s: &VectorFileStatus) -> String {
    match s {
        VectorFileStatus::InProgress => "in_progress".to_owned(),
        VectorFileStatus::Completed => "completed".to_owned(),
        VectorFileStatus::Failed(r) => r.clone(),
    }
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod attachments_tests;
