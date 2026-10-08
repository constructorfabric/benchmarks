//! Attachment upload (DESIGN §3.3 "Upload Attachment", §3.6 "File Upload",
//! "Creation protocol" of `chat_vector_stores`).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use tokio::time::Instant;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz::actions;
use crate::domain::error::{DisabledFeature, DomainError, DomainResult, retry_contention};
use crate::domain::thumbnail;
use crate::domain::time::now;
use crate::infra::db::entities::{attachment, chat};
use crate::infra::db::repo;
use crate::infra::llm::ResolvedProvider;
use crate::infra::llm::storage::IndexingStatus;
use crate::infra::outbox::{AttachmentCleanupPayload, Wakes};

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const OCTET_STREAM: &str = "application/octet-stream";

/// Synchronous indexing deadline, counted from the start of the upload request.
pub const SYNC_INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
pub const BACKGROUND_INDEXING_LIMIT: Duration = Duration::from_secs(600);
/// Background indexing heartbeat round (refreshes `updated_at`).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
/// A NULL vector-store placeholder older than this is reclaimed.
pub const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);

const _: () = assert!(
    BACKGROUND_ROUND.as_secs() * 2 <= 60,
    "heartbeat must be at most half of min stale_after_secs"
);

const SUPPORTED: &[&str] = &[
    "application/pdf",
    DOCX,
    PPTX,
    XLSX,
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-script.python",
    "application/x-python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "application/x-javascript",
    "text/x-typescript",
    "application/typescript",
    "application/x-typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/x-ruby",
    "application/sql",
    "text/x-sql",
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];

/// MIME type for a filename extension (used for `application/octet-stream` parts).
#[must_use]
pub fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => DOCX,
        "pptx" => PPTX,
        "xlsx" => XLSX,
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/x-typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Defaults the filename to `upload` and truncates it to 255 characters, keeping the extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("upload");
    let n = name.chars().count();
    if n <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.chars().count() < 64 => {
            let ext_len = ext.chars().count() + 1;
            let stem: String = name.chars().take(255 - ext_len).collect();
            format!("{stem}.{ext}")
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Resolved MIME type and purposes of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadClass {
    pub content_type: String,
    /// `document` or `image`.
    pub kind: &'static str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub limit_bytes: u64,
}

/// Chat, model and kill switches resolved before the body is read.
#[derive(Debug, Clone)]
pub struct UploadTarget {
    pub chat: chat::Model,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
}

/// Validates the MIME type and derives kind, purposes and the size limit.
///
/// # Errors
/// `UnsupportedContentType`, `FeatureDisabled(Images)`, `CodeInterpreterUnavailable`.
pub fn classify(
    raw_content_type: &str,
    filename: &str,
    allow_csv: bool,
    file_max_kb: u32,
    image_max_kb: u32,
    target: &UploadTarget,
) -> DomainResult<UploadClass> {
    let mut ct = raw_content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if ct == OCTET_STREAM
        && let Some(m) = mime_from_extension(filename)
    {
        m.clone_into(&mut ct);
    }
    if ct == "text/csv" || ct == "application/csv" {
        if !allow_csv {
            return Err(DomainError::UnsupportedContentType(ct));
        }
        "text/plain".clone_into(&mut ct);
    }
    if !SUPPORTED.contains(&ct.as_str()) {
        return Err(DomainError::UnsupportedContentType(ct));
    }
    let is_image = ct.starts_with("image/");
    let model_cap = u64::from(target.model.general_config.max_file_size_mb) * 1024 * 1024;
    let cfg_cap = u64::from(if is_image { image_max_kb } else { file_max_kb }) * 1024;
    let limit_bytes = if model_cap == 0 {
        cfg_cap
    } else {
        cfg_cap.min(model_cap)
    };
    if is_image {
        if target.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled(DisabledFeature::Images));
        }
        return Ok(UploadClass {
            content_type: ct,
            kind: "image",
            for_file_search: false,
            for_code_interpreter: false,
            limit_bytes,
        });
    }
    let mut for_code_interpreter = ct == XLSX;
    let for_file_search = ct != XLSX;
    let ci_available = !target.kill_switches.disable_code_interpreter
        && target.model.general_config.tool_support.code_interpreter;
    if for_code_interpreter && !ci_available {
        if !for_file_search {
            return Err(DomainError::CodeInterpreterUnavailable);
        }
        for_code_interpreter = false;
    }
    Ok(UploadClass {
        content_type: ct,
        kind: "document",
        for_file_search,
        for_code_interpreter,
        limit_bytes,
    })
}

/// Provider-side filename: `{chat_id}_{attachment_id}.{ext}` (DESIGN "Provider-side
/// orphan files"), keeping the extension of the uploaded filename.
#[must_use]
pub fn provider_filename(row: &attachment::Model) -> String {
    let ext =
        row.filename.rsplit_once('.').map(|(_, e)| e).filter(|e| {
            !e.is_empty() && e.len() <= 16 && e.chars().all(|c| c.is_ascii_alphanumeric())
        });
    match ext {
        Some(e) => format!("{}_{}.{}", row.chat_id, row.id, e.to_ascii_lowercase()),
        None => format!("{}_{}", row.chat_id, row.id),
    }
}

/// Next poll delay: doubling up to `max`.
fn next_delay(d: Duration, max: Duration) -> Duration {
    (d * 2).min(max)
}

enum PollOutcome {
    Completed,
    Failed(String),
    Deadline,
}

impl AppServices {
    /// Authorization, chat and model resolution before the body is read.
    ///
    /// # Errors
    /// `NotFound(Chat)`, `InvalidModel`, authorization and policy errors.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> DomainResult<UploadTarget> {
        let chat = self
            .authorized_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id)
            .await?;
        let snapshot = self.chat_snapshot(ctx.subject_id(), &chat).await?;
        let model = snapshot.find(&chat.model).cloned().ok_or_else(|| {
            DomainError::InvalidModel(format!(
                "the chat model '{}' is no longer available",
                chat.model
            ))
        })?;
        Ok(UploadTarget {
            chat,
            model,
            kill_switches: snapshot.kill_switches,
        })
    }

    /// Validates the part's MIME type for the target chat.
    ///
    /// # Errors
    /// See [`classify`].
    pub fn classify_upload(
        &self,
        content_type: &str,
        filename: &str,
        target: &UploadTarget,
    ) -> DomainResult<UploadClass> {
        let rag = &self.cfg.rag;
        classify(
            content_type,
            filename,
            rag.allow_csv_upload,
            rag.uploaded_file_max_size_kb,
            rag.uploaded_image_max_size_kb,
            target,
        )
    }

    /// Stores an uploaded file: row insert with per-chat limits, provider upload,
    /// vector-store indexing or thumbnail, final status.
    ///
    /// # Errors
    /// `DocumentLimit`, `StorageLimit`, `ProviderMismatch`, `StorageUnavailable`, DB errors.
    #[allow(clippy::too_many_lines)]
    pub async fn store_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        target: UploadTarget,
        class: UploadClass,
        filename: String,
        data: Bytes,
        started: Instant,
    ) -> DomainResult<attachment::Model> {
        let chat = &target.chat;
        let rag_provider = self
            .llm
            .resolve_rag(&target.model.provider_id, chat.tenant_id)?;
        let backend = rag_provider.storage_backend.clone();
        if class.for_file_search {
            let conn = self.db.conn()?;
            if let Some(vs) = repo::find_vector_store(&conn, chat.tenant_id, chat.id).await?
                && vs.provider != backend
            {
                return Err(DomainError::ProviderMismatch);
            }
        }
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let ts = now();
        let row = attachment::Model {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            uploaded_by_user_id: ctx.subject_id(),
            filename: filename.clone(),
            content_type: class.content_type.clone(),
            size_bytes: size,
            storage_backend: backend.clone(),
            provider_file_id: None,
            status: "pending".to_owned(),
            error_code: None,
            attachment_kind: class.kind.to_owned(),
            for_file_search: class.for_file_search,
            for_code_interpreter: class.for_code_interpreter,
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
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
            secondary_provider_kind: None,
        };
        let max_docs = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let is_document = class.kind == "document";
        let inserted = retry_contention(|| {
            let insert_row = row.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let existing =
                        repo::chat_attachments(tx, insert_row.tenant_id, insert_row.chat_id)
                            .await?;
                    let live: Vec<&attachment::Model> =
                        existing.iter().filter(|a| a.status != "failed").collect();
                    let docs = live
                        .iter()
                        .filter(|a| a.attachment_kind == "document")
                        .count() as u64;
                    if is_document && docs >= max_docs {
                        return Err(DomainError::DocumentLimit);
                    }
                    let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                    if total.saturating_add(insert_row.size_bytes) > max_total {
                        return Err(DomainError::StorageLimit);
                    }
                    repo::insert_attachment(tx, insert_row).await
                })
            })
        })
        .await;
        let row = match inserted {
            Ok(r) => r,
            Err(e) => {
                let result = match &e {
                    DomainError::DocumentLimit => "document_limit",
                    DomainError::StorageLimit => "storage_limit",
                    _ => "error",
                };
                self.metrics.inc(
                    "attachment_upload",
                    &[("kind", class.kind), ("result", result)],
                );
                return Err(e);
            }
        };
        self.metrics.updown("attachments_pending", 1);
        let res = self
            .process_upload(&row, &class, &rag_provider, data, started)
            .await;
        self.metrics.updown("attachments_pending", -1);
        let result = if res.is_ok() { "ok" } else { "error" };
        self.metrics.inc(
            "attachment_upload",
            &[("kind", class.kind), ("result", result)],
        );
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "attachment_upload_bytes",
            size as f64,
            &[("kind", class.kind)],
        );
        res
    }

    async fn mark_failed(&self, row: &attachment::Model, code: &str) {
        let ts = now();
        let Ok(conn) = self.db.conn() else { return };
        let r = repo::update_attachment_where(
            &conn,
            row.tenant_id,
            row.id,
            Condition::all().add(attachment::Column::Status.is_in(["pending", "uploaded"])),
            vec![
                (attachment::Column::Status, Expr::value("failed")),
                (attachment::Column::ErrorCode, Expr::value(code.to_owned())),
                (attachment::Column::UpdatedAt, Expr::value(ts)),
            ],
        )
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, attachment_id = %row.id, "failed to mark the attachment failed");
        }
    }

    fn delete_file_best_effort(self: &Arc<Self>, provider: &ResolvedProvider, file_id: String) {
        let svc = Arc::clone(self);
        let provider = provider.clone();
        tokio::spawn(async move {
            if let Err(e) = svc.llm.delete_file(&provider, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    async fn reload(&self, row: &attachment::Model) -> DomainResult<attachment::Model> {
        let conn = self.db.conn()?;
        repo::find_attachment_by_id(&conn, row.tenant_id, row.id)
            .await?
            .ok_or_else(|| DomainError::internal("attachment row disappeared"))
    }

    #[allow(clippy::too_many_lines)]
    async fn process_upload(
        self: &Arc<Self>,
        row: &attachment::Model,
        class: &UploadClass,
        provider: &ResolvedProvider,
        data: Bytes,
        started: Instant,
    ) -> DomainResult<attachment::Model> {
        let file_id = match self
            .llm
            .upload_file(
                provider,
                &provider_filename(row),
                &row.content_type,
                data.clone(),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.mark_failed(row, "upload_failed").await;
                return Err(DomainError::StorageUnavailable(format!(
                    "file upload failed: {e}"
                )));
            }
        };
        let ts = now();
        {
            let conn = self.db.conn()?;
            repo::update_attachment_where(
                &conn,
                row.tenant_id,
                row.id,
                Condition::all().add(attachment::Column::Status.eq("pending")),
                vec![
                    (attachment::Column::Status, Expr::value("uploaded")),
                    (
                        attachment::Column::ProviderFileId,
                        Expr::value(file_id.clone()),
                    ),
                    (attachment::Column::UpdatedAt, Expr::value(ts)),
                ],
            )
            .await?;
        }
        if class.kind == "image" {
            let cfg = self.cfg.thumbnail.clone();
            let thumb = tokio::task::spawn_blocking(move || thumbnail::generate(&data, &cfg))
                .await
                .ok()
                .flatten();
            let mut cols = vec![
                (attachment::Column::Status, Expr::value("ready")),
                (attachment::Column::UpdatedAt, Expr::value(now())),
            ];
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
            let conn = self.db.conn()?;
            repo::update_attachment_where(
                &conn,
                row.tenant_id,
                row.id,
                Condition::all().add(attachment::Column::Status.eq("uploaded")),
                cols,
            )
            .await?;
            return self.reload(row).await;
        }
        if !class.for_file_search {
            let conn = self.db.conn()?;
            repo::update_attachment_where(
                &conn,
                row.tenant_id,
                row.id,
                Condition::all().add(attachment::Column::Status.eq("uploaded")),
                vec![
                    (attachment::Column::Status, Expr::value("ready")),
                    (attachment::Column::UpdatedAt, Expr::value(now())),
                ],
            )
            .await?;
            return self.reload(row).await;
        }

        let vs_id = match self
            .ensure_vector_store(row.tenant_id, row.chat_id, provider)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.mark_failed(row, "vector_store_failed").await;
                self.delete_file_best_effort(provider, file_id);
                return Err(e);
            }
        };
        let deadline = started + SYNC_INDEXING_DEADLINE;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(100));
        let first = self
            .llm
            .add_vector_store_file(provider, &vs_id, &file_id, &row.id.to_string(), remaining)
            .await;
        let outcome = match first {
            Ok(IndexingStatus::Completed) => PollOutcome::Completed,
            Ok(IndexingStatus::Failed(s)) => PollOutcome::Failed(format!("indexing status '{s}'")),
            Ok(IndexingStatus::InProgress) => {
                self.poll_sync(provider, &vs_id, &file_id, deadline).await
            }
            Err(e) => PollOutcome::Failed(format!("add to vector store failed: {e}")),
        };
        match outcome {
            PollOutcome::Completed => {
                let conn = self.db.conn()?;
                repo::update_attachment_where(
                    &conn,
                    row.tenant_id,
                    row.id,
                    Condition::all().add(attachment::Column::Status.eq("uploaded")),
                    vec![
                        (attachment::Column::Status, Expr::value("ready")),
                        (attachment::Column::UpdatedAt, Expr::value(now())),
                    ],
                )
                .await?;
                self.reload(row).await
            }
            PollOutcome::Failed(detail) => {
                self.mark_failed(row, "indexing_failed").await;
                self.delete_file_best_effort(provider, file_id);
                Err(DomainError::StorageUnavailable(detail))
            }
            PollOutcome::Deadline => {
                let svc = Arc::clone(self);
                let row2 = row.clone();
                let provider = provider.clone();
                tokio::spawn(async move {
                    svc.background_indexing(row2, provider, vs_id, file_id)
                        .await;
                });
                self.reload(row).await
            }
        }
    }

    async fn poll_sync(
        &self,
        provider: &ResolvedProvider,
        vs_id: &str,
        file_id: &str,
        deadline: Instant,
    ) -> PollOutcome {
        let mut delay = Duration::from_millis(250);
        loop {
            let now_i = Instant::now();
            if now_i >= deadline {
                return PollOutcome::Deadline;
            }
            tokio::time::sleep(delay.min(deadline - now_i)).await;
            delay = next_delay(delay, Duration::from_secs(2));
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return PollOutcome::Deadline;
            }
            match self
                .llm
                .vector_store_file_status(provider, vs_id, file_id, remaining)
                .await
            {
                Ok(IndexingStatus::Completed) => return PollOutcome::Completed,
                Ok(IndexingStatus::InProgress) => {}
                Ok(IndexingStatus::Failed(s)) => {
                    return PollOutcome::Failed(format!("indexing status '{s}'"));
                }
                Err(e) if e.is_transient() => {
                    tracing::debug!(error = %e, "transient indexing status read error");
                }
                Err(e) => return PollOutcome::Failed(format!("indexing status read failed: {e}")),
            }
        }
    }

    /// Get-or-create of the chat's vector store (placeholder protocol).
    // Bounded retry loop of the placeholder protocol; the steps read best in one place.
    #[allow(clippy::cognitive_complexity)]
    async fn ensure_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &ResolvedProvider,
    ) -> DomainResult<String> {
        let backend = provider.storage_backend.as_str();
        for _ in 0..3 {
            let existing = {
                let conn = self.db.conn()?;
                repo::find_vector_store(&conn, tenant_id, chat_id).await?
            };
            if let Some(vs) = existing {
                if vs.provider != backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = vs.vector_store_id {
                    return Ok(id);
                }
                let age = now() - vs.created_at;
                if age
                    > time::Duration::seconds(
                        i64::try_from(STALE_PLACEHOLDER.as_secs()).unwrap_or(120),
                    )
                {
                    let conn = self.db.conn()?;
                    repo::delete_vector_store_placeholder(&conn, tenant_id, vs.id).await?;
                    continue;
                }
                return self.await_vector_store(tenant_id, chat_id, backend).await;
            }
            let inserted = {
                let conn = self.db.conn()?;
                repo::insert_vector_store_placeholder(&conn, tenant_id, chat_id, backend, now())
                    .await
            };
            let row_id = match inserted {
                Ok(id) => id,
                Err(e) if e.is_unique_violation() => {
                    return self.await_vector_store(tenant_id, chat_id, backend).await;
                }
                Err(e) => return Err(e),
            };
            let created = self
                .llm
                .create_vector_store(provider, &format!("mini-chat-{chat_id}"))
                .await;
            let vs_id = match created {
                Ok(id) => id,
                Err(e) => {
                    if let Ok(conn) = self.db.conn()
                        && let Err(del_err) =
                            repo::delete_vector_store_row(&conn, tenant_id, row_id).await
                    {
                        tracing::debug!(error = %del_err, "best-effort delete of the vector store row failed");
                    }
                    return Err(DomainError::StorageUnavailable(format!(
                        "vector store create failed: {e}"
                    )));
                }
            };
            let won = {
                let conn = self.db.conn()?;
                repo::set_vector_store_id(&conn, tenant_id, row_id, &vs_id).await?
            };
            if won == 1 {
                return Ok(vs_id);
            }
            if let Err(e) = self.llm.delete_vector_store(provider, &vs_id).await {
                tracing::warn!(error = %e, "best-effort delete of a superseded vector store failed");
            }
            return self.await_vector_store(tenant_id, chat_id, backend).await;
        }
        Err(DomainError::StorageUnavailable(
            "vector store creation did not converge".to_owned(),
        ))
    }

    /// Loser path: polls the row until its `vector_store_id` is set (5 polls).
    async fn await_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        backend: &str,
    ) -> DomainResult<String> {
        let mut delay = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(delay).await;
            delay *= 2;
            let conn = self.db.conn()?;
            if let Some(vs) = repo::find_vector_store(&conn, tenant_id, chat_id).await? {
                if vs.provider != backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = vs.vector_store_id {
                    return Ok(id);
                }
            }
        }
        Err(DomainError::StorageUnavailable(
            "the chat vector store is being created by another upload".to_owned(),
        ))
    }

    /// Guard for background writes: still `uploaded`, not deleted, not owned by chat cleanup.
    fn uploaded_guard() -> Condition {
        Condition::all()
            .add(attachment::Column::Status.eq("uploaded"))
            .add(attachment::Column::DeletedAt.is_null())
            .add(attachment::Column::CleanupStatus.is_null())
    }

    /// Background indexing wait for a document returned as `uploaded`.
    // Single polling state machine; splitting it would scatter the terminal-state handling.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub(crate) async fn background_indexing(
        self: Arc<Self>,
        row: attachment::Model,
        provider: ResolvedProvider,
        vs_id: String,
        file_id: String,
    ) {
        let limit = Instant::now() + BACKGROUND_INDEXING_LIMIT;
        let mut last_transient: Option<String> = None;
        let outcome = 'outer: loop {
            if Instant::now() >= limit {
                break 'outer Err("timeout".to_owned());
            }
            // Heartbeat: refresh updated_at; stop when the row is no longer ours.
            let refreshed = match self.db.conn() {
                Ok(conn) => repo::update_attachment_where(
                    &conn,
                    row.tenant_id,
                    row.id,
                    Self::uploaded_guard(),
                    vec![(attachment::Column::UpdatedAt, Expr::value(now()))],
                )
                .await
                .unwrap_or(1),
                Err(_) => 1,
            };
            if refreshed == 0 {
                return;
            }
            let round_end = (Instant::now() + BACKGROUND_ROUND).min(limit);
            let mut delay = Duration::from_millis(250);
            while Instant::now() < round_end {
                let wait = delay.min(round_end.saturating_duration_since(Instant::now()));
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                delay = next_delay(delay, Duration::from_secs(5));
                let r = tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    r = self.llm.vector_store_file_status(&provider, &vs_id, &file_id, Duration::from_secs(20)) => r,
                };
                match r {
                    Ok(IndexingStatus::Completed) => break 'outer Ok(()),
                    Ok(IndexingStatus::InProgress) => {}
                    Ok(IndexingStatus::Failed(s)) => {
                        break 'outer Err(format!("indexing status '{s}'"));
                    }
                    Err(e) if e.is_transient() => {
                        if last_transient.is_none() {
                            tracing::warn!(error = %e, attachment_id = %row.id, "transient indexing status read error");
                        }
                        last_transient = Some(e.to_string());
                    }
                    Err(e) => break 'outer Err(format!("indexing status read failed: {e}")),
                }
            }
        };
        match outcome {
            Ok(()) => {
                let mut wait = Duration::from_secs(1);
                for attempt in 0..4 {
                    let r = match self.db.conn() {
                        Ok(conn) => {
                            repo::update_attachment_where(
                                &conn,
                                row.tenant_id,
                                row.id,
                                Self::uploaded_guard(),
                                vec![
                                    (attachment::Column::Status, Expr::value("ready")),
                                    (attachment::Column::UpdatedAt, Expr::value(now())),
                                ],
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    };
                    match r {
                        Ok(1) => {
                            self.metrics
                                .inc("attachment_background_indexing", &[("result", "ready")]);
                            return;
                        }
                        Ok(_) => return,
                        Err(e) => {
                            tracing::warn!(error = %e, attempt, "setting the attachment ready failed");
                            if attempt < 3 {
                                tokio::time::sleep(wait).await;
                                wait *= 2;
                            }
                        }
                    }
                }
                self.metrics.inc(
                    "attachment_background_indexing",
                    &[("result", "set_ready_failed")],
                );
            }
            Err(reason) => {
                let timeout = reason == "timeout";
                if timeout {
                    tracing::warn!(attachment_id = %row.id, last_error = ?last_transient, "background indexing timed out");
                } else {
                    tracing::warn!(attachment_id = %row.id, reason = %reason, "background indexing failed");
                }
                let outbox = Arc::clone(&self.outbox);
                let r = self
                    .db
                    .transaction(move |tx| {
                        Box::pin(async move {
                            let ts = now();
                            let n = repo::update_attachment_where(
                                tx,
                                row.tenant_id,
                                row.id,
                                Self::uploaded_guard(),
                                vec![
                                    (attachment::Column::Status, Expr::value("failed")),
                                    (
                                        attachment::Column::ErrorCode,
                                        Expr::value("indexing_failed"),
                                    ),
                                    (attachment::Column::CleanupStatus, Expr::value("pending")),
                                    (attachment::Column::CleanupUpdatedAt, Expr::value(ts)),
                                    (attachment::Column::UpdatedAt, Expr::value(ts)),
                                ],
                            )
                            .await?;
                            let mut wakes = Wakes::default();
                            if n == 0 {
                                return Ok(None);
                            }
                            let payload = AttachmentCleanupPayload {
                                event_type: "attachment_indexing_failed".to_owned(),
                                tenant_id: row.tenant_id,
                                chat_id: row.chat_id,
                                attachment_id: row.id,
                                provider_file_id: Some(file_id),
                                vector_store_id: None,
                                storage_backend: row.storage_backend.clone(),
                                attachment_kind: row.attachment_kind.clone(),
                                deleted_at: ts,
                                secondary_ref: None,
                            };
                            wakes.push(
                                outbox
                                    .attachment_cleanup(tx, &payload)
                                    .await
                                    .map_err(crate::domain::turns::internal_payload)?,
                            );
                            Ok(Some(wakes))
                        })
                    })
                    .await;
                match r {
                    Ok(Some(w)) => {
                        w.fire();
                        let label = if timeout { "timeout" } else { "failed" };
                        self.metrics
                            .inc("attachment_background_indexing", &[("result", label)]);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to record the background indexing failure");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod upload_tests;
