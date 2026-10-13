//! Attachments (DESIGN §3.6 "File Upload", §4 "Attachment Deletion",
//! ADR-0007): synchronous upload with streaming size checks, per-chat limits,
//! purpose routing, vector-store indexing (25 s in the request, then up to 10
//! minutes in the background), thumbnails, deletion through the outbox.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::{DomainError, FeatureSubject};
use crate::domain::models::attachment_kind;
use crate::infra::db::entities::{attachments, chat_vector_stores, chats};
use crate::infra::db::repo;
use crate::infra::db::repo::attachments::status;
use crate::infra::llm::StorageTarget;
use crate::infra::llm::storage::IndexStatus;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

/// XLSX MIME type (code interpreter only).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    XLSX,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "text/x-typescript",
    "application/typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "text/x-sql",
];

/// Synchronous indexing deadline from the start of the upload.
pub const SYNC_INDEX_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
pub const BACKGROUND_INDEX_LIMIT: Duration = Duration::from_secs(600);
/// Background heartbeat round.
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);

/// MIME type inferred from a filename extension.
#[must_use]
pub fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" => "text/plain",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" => "text/javascript",
        "ts" => "text/x-typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Normalizes the declared MIME type of the upload.
///
/// # Errors
/// `UnsupportedContentType`.
pub fn resolve_mime(declared: &str, filename: &str, allow_csv: bool) -> Result<String, DomainError> {
    let mut mime = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime == "application/octet-stream" {
        mime_from_extension(filename)
            .unwrap_or("application/octet-stream")
            .clone_into(&mut mime);
    }
    if mime == "image/jpg" {
        "image/jpeg".clone_into(&mut mime);
    }
    if mime == "text/csv" {
        if allow_csv {
            "text/plain".clone_into(&mut mime);
        } else {
            return Err(DomainError::UnsupportedContentType);
        }
    }
    if IMAGE_TYPES.contains(&mime.as_str()) || DOCUMENT_TYPES.contains(&mime.as_str()) {
        Ok(mime)
    } else {
        Err(DomainError::UnsupportedContentType)
    }
}

/// Truncates a filename to 255 characters keeping the extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    if name.chars().count() <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 250 => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{ext}", stem.chars().take(keep).collect::<String>())
        }
        _ => name.chars().take(255).collect(),
    }
}

fn extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 10 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".to_owned())
}

/// Multipart error helper.
fn mp(field: &'static str, reason: &'static str, detail: impl Into<String>) -> DomainError {
    DomainError::Multipart {
        field,
        reason,
        detail: detail.into(),
    }
}

/// Upload stream input.
pub struct UploadInput<S> {
    pub content_type: Option<String>,
    pub body: S,
}

impl MiniChatService {
    /// Uploads an attachment.
    ///
    /// # Errors
    /// See DESIGN upload error table.
    #[allow(clippy::too_many_lines)]
    pub async fn upload_attachment<S, E>(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: UploadInput<S>,
    ) -> Result<attachments::Model, DomainError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
    {
        let started = tokio::time::Instant::now();
        let (_, chat) = self.authorized_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = snapshot.find(&chat.model).cloned().ok_or(DomainError::InvalidModel)?;
        let target = self
            .llm
            .resolver()
            .storage_for(&model.provider_id, chat.tenant_id)
            .ok_or_else(|| DomainError::Internal(format!("no storage provider for '{}'", model.provider_id)))?;
        let Ok(_permit) = Arc::clone(&self.upload_slots).try_acquire_owned() else {
            return Err(DomainError::UploadConcurrency);
        };
        // Multipart parsing.
        let ct = input.content_type.unwrap_or_default();
        let boundary = multer::parse_boundary(&ct)
            .map_err(|e| mp("content_type", "BOUNDARY_REQUIRED", e.to_string()))?;
        let mut multipart = multer::Multipart::new(input.body, boundary);
        let mut field = loop {
            match multipart.next_field().await {
                Ok(Some(f)) if f.name() == Some("file") => break f,
                Ok(Some(_)) => {}
                Ok(None) => return Err(mp("file", "MISSING_FILE", "multipart body has no 'file' field")),
                Err(e) => return Err(mp("multipart", "MULTIPART_ERROR", e.to_string())),
            }
        };
        let declared = field
            .content_type()
            .map(ToString::to_string)
            .ok_or_else(|| mp("content_type", "MISSING_CONTENT_TYPE", "the 'file' part has no content type"))?;
        let filename = normalize_filename(field.file_name());
        let mime = resolve_mime(&declared, &filename, self.cfg.rag.allow_csv_upload)?;
        let is_image = IMAGE_TYPES.contains(&mime.as_str());
        if is_image && snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled(FeatureSubject::Images));
        }
        let mut for_file_search = !is_image && mime != XLSX;
        let mut for_code_interpreter = mime == XLSX;
        if for_code_interpreter
            && (snapshot.kill_switches.disable_code_interpreter
                || !model.general_config.tool_support.code_interpreter)
        {
            for_code_interpreter = false;
            if !for_file_search {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
        }
        let _ = &mut for_file_search;
        let kind_limit_kb = if is_image {
            self.cfg.rag.uploaded_image_max_size_kb
        } else {
            self.cfg.rag.uploaded_file_max_size_kb
        };
        let mut limit = u64::from(kind_limit_kb) * 1024;
        let model_mb = model.general_config.max_file_size_mb;
        if model_mb > 0 {
            limit = limit.min(u64::from(model_mb) * 1024 * 1024);
        }
        let mut data: Vec<u8> = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if (data.len() + chunk.len()) as u64 > limit {
                        return Err(DomainError::FileTooLarge);
                    }
                    data.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(mp("multipart", "MULTIPART_ERROR", e.to_string())),
            }
        }
        drop(field);
        let size = data.len();
        let kind = if is_image {
            attachment_kind::IMAGE
        } else {
            attachment_kind::DOCUMENT
        };
        #[allow(clippy::cast_precision_loss)] // metrics value only
        self.metrics
            .record("attachment_upload_bytes", size as f64, &[("kind", kind.to_owned())]);
        let att_id = Uuid::new_v4();
        let row = self
            .insert_pending(&chat, ctx.subject_id(), att_id, &filename, &mime, kind, size, for_file_search, for_code_interpreter, &target)
            .await?;
        let result = self
            .process_upload(ctx, &chat, row, &target, Bytes::from(data), started)
            .await;
        self.metrics.inc(
            "attachment_upload",
            1,
            &[("kind", kind.to_owned()), ("result", if result.is_ok() { "ok" } else { "error" }.to_owned())],
        );
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_pending(
        self: &Arc<Self>,
        chat: &chats::Model,
        user: Uuid,
        att_id: Uuid,
        filename: &str,
        mime: &str,
        kind: &'static str,
        size: usize,
        for_file_search: bool,
        for_code_interpreter: bool,
        target: &StorageTarget,
    ) -> Result<attachments::Model, DomainError> {
        let rag = self.cfg.rag.clone();
        let chat = chat.clone();
        let filename = filename.to_owned();
        let mime = mime.to_owned();
        let label = target.backend_label.clone();
        self.transact(move |tx| {
            Box::pin(async move {
                let existing = repo::attachments::list_for_chat(tx, chat.tenant_id, chat.id).await?;
                let live: Vec<_> = existing.iter().filter(|a| a.status != status::FAILED).collect();
                if kind == attachment_kind::DOCUMENT {
                    let docs = live.iter().filter(|a| a.attachment_kind == attachment_kind::DOCUMENT).count();
                    if docs >= usize::try_from(rag.max_documents_per_chat).unwrap_or(usize::MAX) {
                        return Err(DomainError::DocumentLimit);
                    }
                }
                let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                let cap = i64::from(rag.max_total_upload_mb_per_chat) * 1024 * 1024;
                if total + i64::try_from(size).unwrap_or(i64::MAX) > cap {
                    return Err(DomainError::StorageLimit);
                }
                if for_file_search
                    && let Some(vs) = repo::vector_stores::find(tx, chat.tenant_id, chat.id).await?
                    && vs.provider != label
                {
                    return Err(DomainError::ProviderMismatch);
                }
                let now = clock::now();
                let model = attachments::Model {
                    id: att_id,
                    tenant_id: chat.tenant_id,
                    chat_id: chat.id,
                    uploaded_by_user_id: user,
                    filename,
                    content_type: mime,
                    size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
                    storage_backend: label,
                    provider_file_id: None,
                    status: status::PENDING.to_owned(),
                    error_code: None,
                    attachment_kind: kind.to_owned(),
                    for_file_search,
                    for_code_interpreter,
                    doc_summary: None,
                    img_thumbnail: None,
                    img_thumbnail_width: None,
                    img_thumbnail_height: None,
                    summary_model: None,
                    summary_updated_at: None,
                    cleanup_status: None,
                    cleanup_attempts: 0,
                    last_cleanup_error: None,
                    cleanup_updated_at: None,
                    created_at: now,
                    updated_at: now,
                    deleted_at: None,
                    secondary_file_id: None,
                    secondary_status: "not_attempted".to_owned(),
                    secondary_provider_kind: None,
                };
                let am: attachments::ActiveModel = model.clone().into();
                repo::attachments::insert(tx, chat.tenant_id, am).await?;
                Ok((model, toolkit_db::outbox::Wake::empty()))
            })
        })
        .await
    }

    async fn set_fields(
        &self,
        row: &attachments::Model,
        sets: Vec<(attachments::Column, sea_orm::sea_query::SimpleExpr)>,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        repo::attachments::update(&conn, row.tenant_id, row.id, sets, None).await?;
        Ok(())
    }

    async fn fail_row(&self, row: &attachments::Model, code: &str) {
        if let Ok(conn) = self.db.conn() {
            repo::attachments::mark_failed(&conn, row.tenant_id, row.id, code, clock::now()).await.ok();
        }
    }

    async fn reload(&self, row: &attachments::Model) -> Result<attachments::Model, DomainError> {
        let conn = self.db.conn()?;
        repo::attachments::find_by_id(&conn, row.id)
            .await?
            .ok_or(DomainError::AttachmentNotFound)
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn process_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat: &chats::Model,
        row: attachments::Model,
        target: &StorageTarget,
        data: Bytes,
        started: tokio::time::Instant,
    ) -> Result<attachments::Model, DomainError> {
        let provider_name = format!("{}_{}.{}", chat.id, row.id, extension(&row.filename));
        let file_id = match self
            .storage
            .upload_file(ctx, target, &provider_name, &row.content_type, data.clone())
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %row.id, "provider file upload failed");
                self.fail_row(&row, "upload_failed").await;
                return Err(DomainError::StorageUnavailable(e.message));
            }
        };
        self.set_fields(
            &row,
            vec![
                (attachments::Column::Status, Expr::value(status::UPLOADED)),
                (attachments::Column::ProviderFileId, Expr::value(file_id.clone())),
                (attachments::Column::UpdatedAt, Expr::value(clock::now())),
            ],
        )
        .await?;
        if row.attachment_kind == attachment_kind::IMAGE {
            let thumb = if data.len() <= self.cfg.thumbnail.max_decode_bytes {
                let cfg = self.cfg.thumbnail.clone();
                let d = data.clone();
                tokio::task::spawn_blocking(move || crate::infra::thumbnail::generate(&d, &cfg))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let mut sets = vec![
                (attachments::Column::Status, Expr::value(status::READY)),
                (attachments::Column::UpdatedAt, Expr::value(clock::now())),
            ];
            if let Some(t) = thumb {
                sets.push((attachments::Column::ImgThumbnail, Expr::value(t.data)));
                sets.push((attachments::Column::ImgThumbnailWidth, Expr::value(i32::try_from(t.width).unwrap_or(0))));
                sets.push((attachments::Column::ImgThumbnailHeight, Expr::value(i32::try_from(t.height).unwrap_or(0))));
            }
            self.set_fields(&row, sets).await?;
            return self.reload(&row).await;
        }
        if !row.for_file_search {
            self.set_fields(
                &row,
                vec![
                    (attachments::Column::Status, Expr::value(status::READY)),
                    (attachments::Column::UpdatedAt, Expr::value(clock::now())),
                ],
            )
            .await?;
            return self.reload(&row).await;
        }
        // Document with file_search: vector store.
        let vs_id = match self.ensure_vector_store(ctx, chat, target).await {
            Ok(id) => id,
            Err(e) => {
                self.fail_row(&row, "vector_store_failed").await;
                self.storage.delete_file(ctx, target, &file_id).await.ok();
                return Err(e);
            }
        };
        let first = self
            .storage
            .add_file_to_vector_store(ctx, target, &vs_id, &file_id, row.id)
            .await;
        let mut st = match first {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %row.id, "vector store file add failed");
                self.fail_row(&row, "indexing_failed").await;
                self.storage.delete_file(ctx, target, &file_id).await.ok();
                return Err(DomainError::StorageUnavailable(e.message));
            }
        };
        let mut wait = Duration::from_millis(250);
        while st == IndexStatus::InProgress {
            let deadline = started + self.timings.sync_index_deadline;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(wait.min(deadline - now)).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            match tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                self.storage.get_vector_store_file(ctx, target, &vs_id, &file_id),
            )
            .await
            {
                Ok(Ok(s)) => st = s,
                Ok(Err(e)) if e.transient => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "vector store status read failed");
                    st = IndexStatus::Failed;
                }
                Err(_) => break,
            }
        }
        match st {
            IndexStatus::Completed => {
                self.set_fields(
                    &row,
                    vec![
                        (attachments::Column::Status, Expr::value(status::READY)),
                        (attachments::Column::UpdatedAt, Expr::value(clock::now())),
                    ],
                )
                .await?;
                self.reload(&row).await
            }
            IndexStatus::Failed => {
                self.fail_row(&row, "indexing_failed").await;
                self.storage.delete_file(ctx, target, &file_id).await.ok();
                Err(DomainError::StorageUnavailable("indexing failed".to_owned()))
            }
            IndexStatus::InProgress => {
                let svc = Arc::clone(self);
                let ctx2 = ctx.clone();
                let target2 = target.clone();
                let row2 = row.clone();
                tokio::spawn(async move {
                    svc.background_index(ctx2, row2, target2, vs_id, file_id).await;
                });
                self.reload(&row).await
            }
        }
    }

    /// Get-or-create protocol of the chat vector store (DESIGN §3.7).
    async fn ensure_vector_store(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat: &chats::Model,
        target: &StorageTarget,
    ) -> Result<String, DomainError> {
        for _round in 0..3 {
            let conn = self.db.conn()?;
            if let Some(vs) = repo::vector_stores::find(&conn, chat.tenant_id, chat.id).await? {
                if vs.provider != target.backend_label {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = vs.vector_store_id {
                    return Ok(id);
                }
                let stale = clock::now() - vs.created_at > chrono::Duration::seconds(120);
                if stale {
                    repo::vector_stores::delete_row(&conn, chat.tenant_id, vs.id).await?;
                    continue;
                }
                // Loser path: poll for the winner's id.
                let mut wait = Duration::from_millis(200);
                for _ in 0..5 {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                    if let Some(id) = repo::vector_stores::find(&conn, chat.tenant_id, chat.id)
                        .await?
                        .and_then(|v| v.vector_store_id)
                    {
                        return Ok(id);
                    }
                }
                return Err(DomainError::StorageUnavailable("vector store creation in progress".to_owned()));
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_stores::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(chat.tenant_id),
                chat_id: Set(chat.id),
                vector_store_id: Set(None),
                provider: Set(target.backend_label.clone()),
                file_count: Set(0),
                created_at: Set(clock::now()),
            };
            match repo::vector_stores::insert_placeholder(&conn, chat.tenant_id, am).await {
                Ok(()) => {}
                Err(DomainError::UniqueViolation) => continue,
                Err(e) => return Err(e),
            }
            let created = self
                .storage
                .create_vector_store(ctx, target, &format!("mini-chat-{}", chat.id))
                .await;
            let vs_id = match created {
                Ok(id) => id,
                Err(e) => {
                    repo::vector_stores::delete_row(&conn, chat.tenant_id, row_id).await.ok();
                    return Err(DomainError::StorageUnavailable(e.message));
                }
            };
            let n = repo::vector_stores::set_vector_store_id(&conn, chat.tenant_id, row_id, &vs_id).await?;
            if n == 1 {
                return Ok(vs_id);
            }
            self.storage.delete_vector_store(ctx, target, &vs_id).await.ok();
        }
        Err(DomainError::StorageUnavailable("vector store unavailable".to_owned()))
    }

    /// Background indexing wait (rounds of 20 s, up to 10 minutes).
    #[allow(clippy::cognitive_complexity)]
    async fn background_index(
        self: Arc<Self>,
        ctx: SecurityContext,
        row: attachments::Model,
        target: StorageTarget,
        vs_id: String,
        file_id: String,
    ) {
        let began = tokio::time::Instant::now();
        let alive = || {
            Condition::all()
                .add(attachments::Column::Status.eq(status::UPLOADED))
                .add(attachments::Column::CleanupStatus.is_null())
                .add(attachments::Column::DeletedAt.is_null())
        };
        loop {
            if began.elapsed() >= self.timings.background_index_limit {
                self.index_failed(&row, "timeout").await;
                return;
            }
            let Ok(conn) = self.db.conn() else { return };
            let touched = repo::attachments::update(
                &conn,
                row.tenant_id,
                row.id,
                vec![(attachments::Column::UpdatedAt, Expr::value(clock::now()))],
                Some(alive()),
            )
            .await
            .unwrap_or(0);
            if touched == 0 {
                return;
            }
            let round_end = tokio::time::Instant::now() + self.timings.background_round;
            let mut wait = Duration::from_millis(250);
            while tokio::time::Instant::now() < round_end {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(Duration::from_secs(5));
                match self.storage.get_vector_store_file(&ctx, &target, &vs_id, &file_id).await {
                    Ok(IndexStatus::Completed) => {
                        for delay in [0_u64, 1, 2, 4] {
                            if delay > 0 {
                                tokio::time::sleep(Duration::from_secs(delay)).await;
                            }
                            let Ok(conn) = self.db.conn() else { continue };
                            if repo::attachments::update(
                                &conn,
                                row.tenant_id,
                                row.id,
                                vec![
                                    (attachments::Column::Status, Expr::value(status::READY)),
                                    (attachments::Column::UpdatedAt, Expr::value(clock::now())),
                                ],
                                Some(alive()),
                            )
                            .await
                            .is_ok()
                            {
                                self.metrics.inc("attachment_background_indexing", 1, &[("result", "ready".to_owned())]);
                                return;
                            }
                        }
                        self.metrics.inc("attachment_background_indexing", 1, &[("result", "set_ready_failed".to_owned())]);
                        return;
                    }
                    Ok(IndexStatus::Failed) => {
                        self.index_failed(&row, "failed").await;
                        return;
                    }
                    Ok(IndexStatus::InProgress) => {}
                    Err(e) if e.transient => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "background status read failed");
                        self.index_failed(&row, "failed").await;
                        return;
                    }
                }
            }
        }
    }

    async fn index_failed(self: &Arc<Self>, row: &attachments::Model, result: &str) {
        let svc = Arc::clone(self);
        let row2 = row.clone();
        let res = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let n = repo::attachments::update(
                        tx,
                        row2.tenant_id,
                        row2.id,
                        vec![
                            (attachments::Column::Status, Expr::value(status::FAILED)),
                            (attachments::Column::ErrorCode, Expr::value("indexing_failed")),
                            (attachments::Column::CleanupStatus, Expr::value("pending")),
                            (attachments::Column::CleanupUpdatedAt, Expr::value(now)),
                            (attachments::Column::UpdatedAt, Expr::value(now)),
                        ],
                        Some(
                            Condition::all()
                                .add(attachments::Column::Status.eq(status::UPLOADED))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        ),
                    )
                    .await?;
                    if n == 0 {
                        return Ok(((), toolkit_db::outbox::Wake::empty()));
                    }
                    let current = repo::attachments::find_by_id(tx, row2.id).await?;
                    let wake = svc
                        .outbox
                        .attachment_cleanup(
                            tx,
                            &AttachmentCleanupPayload {
                                event_type: "attachment_indexing_failed".to_owned(),
                                tenant_id: row2.tenant_id,
                                chat_id: row2.chat_id,
                                attachment_id: row2.id,
                                provider_file_id: current.and_then(|c| c.provider_file_id),
                                vector_store_id: None,
                                storage_backend: row2.storage_backend.clone(),
                                attachment_kind: row2.attachment_kind.clone(),
                                deleted_at: now,
                                secondary_ref: None,
                            },
                        )
                        .await?;
                    Ok(((), wake))
                })
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to record background indexing failure");
        }
        self.metrics
            .inc("attachment_background_indexing", 1, &[("result", result.to_owned())]);
    }

    /// Attachment of the caller in a chat (404 for others).
    ///
    /// # Errors
    /// 404 / authorization errors.
    pub async fn get_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<attachments::Model, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        repo::attachments::find_in_chat(&conn, chat.tenant_id, chat.id, attachment_id, false)
            .await?
            .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::AttachmentNotFound)
    }

    /// Deletes an attachment (idempotent; 409 when referenced).
    ///
    /// # Errors
    /// 404, `AttachmentLocked`, authorization errors.
    pub async fn delete_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::DELETE_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let row = repo::attachments::find_in_chat(&conn, chat.tenant_id, chat.id, attachment_id, true)
            .await?
            .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::AttachmentNotFound)?;
        if row.deleted_at.is_some() {
            return Ok(());
        }
        if repo::messages::attachment_is_referenced(&conn, chat.tenant_id, chat.id, attachment_id).await? {
            return Err(DomainError::AttachmentLocked);
        }
        let svc = Arc::clone(self);
        self.transact(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let n = repo::attachments::soft_delete(tx, row.tenant_id, row.id, now).await?;
                if n == 0 {
                    return Ok(((), toolkit_db::outbox::Wake::empty()));
                }
                let wake = svc
                    .outbox
                    .attachment_cleanup(
                        tx,
                        &AttachmentCleanupPayload {
                            event_type: "attachment_deleted".to_owned(),
                            tenant_id: row.tenant_id,
                            chat_id: row.chat_id,
                            attachment_id: row.id,
                            provider_file_id: row.provider_file_id.clone(),
                            vector_store_id: None,
                            storage_backend: row.storage_backend.clone(),
                            attachment_kind: row.attachment_kind.clone(),
                            deleted_at: now,
                            secondary_ref: None,
                        },
                    )
                    .await
                    .map_err(|e| match e {
                        DomainError::OutboxPayloadTooLarge(m) => DomainError::Internal(m),
                        other => other,
                    })?;
                Ok(((), wake))
            })
        })
        .await
    }
}
