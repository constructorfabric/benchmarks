//! Attachments: upload (synchronous with bounded indexing wait), get,
//! delete, background indexing (DESIGN §3.3 Upload/Get/Delete, §3.6 File
//! Upload, §4 Attachment Deletion, ADR-0007).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sea_orm::EntityTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::state::{AppState, ChatScopes};
use crate::infra::db::entities::{attachments, chat_vector_stores, chats};
use crate::infra::db::repo;
use crate::infra::llm::resolver::StorageTarget;
use crate::infra::outbox::{Queue, Wakes};
use crate::infra::storage::{IndexStatus, StorageError};

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const INDEX_DEADLINE: Duration = Duration::from_secs(25);
const BG_ROUND: Duration = Duration::from_secs(20);
const BG_TOTAL: Duration = Duration::from_secs(600);
const PLACEHOLDER_STALE_SECS: i64 = 120;

/// Supported MIME types and their canonical extensions.
const MIME_TABLE: &[(&str, &[&str])] = &[
    ("application/pdf", &["pdf"]),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", &["docx"]),
    ("application/vnd.openxmlformats-officedocument.presentationml.presentation", &["pptx"]),
    (XLSX, &["xlsx"]),
    ("text/plain", &["txt", "text", "log"]),
    ("text/markdown", &["md", "markdown"]),
    ("text/html", &["html", "htm"]),
    ("application/json", &["json"]),
    ("text/x-python", &["py"]),
    ("text/x-java", &["java"]),
    ("text/javascript", &["js", "mjs"]),
    ("application/javascript", &[]),
    ("application/typescript", &["ts"]),
    ("text/x-typescript", &[]),
    ("text/x-rust", &["rs"]),
    ("text/x-go", &["go"]),
    ("text/x-csharp", &["cs"]),
    ("text/x-ruby", &["rb"]),
    ("application/sql", &["sql"]),
    ("text/x-sql", &[]),
    ("text/x-script.python", &[]),
    ("text/x-java-source", &[]),
    ("image/png", &["png"]),
    ("image/jpeg", &["jpg", "jpeg"]),
    ("image/webp", &["webp"]),
    ("image/gif", &["gif"]),
];

#[must_use]
pub fn is_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif")
}

fn extension(filename: &str) -> Option<String> {
    let (_, ext) = filename.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 16 { None } else { Some(ext.to_ascii_lowercase()) }
}

/// Resolve the effective MIME type (inference for `application/octet-stream`,
/// CSV remap) or `None` when unsupported.
#[must_use]
pub fn resolve_mime(declared: &str, filename: &str, allow_csv: bool) -> Option<String> {
    let base = declared.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let base = if base == "image/jpg" { "image/jpeg".to_owned() } else { base };
    let base = if base == "application/octet-stream" {
        let ext = extension(filename)?;
        if ext == "csv" {
            "text/csv".to_owned()
        } else {
            MIME_TABLE
                .iter()
                .find(|(_, exts)| exts.contains(&ext.as_str()))
                .map(|(m, _)| (*m).to_owned())?
        }
    } else {
        base
    };
    if base == "text/csv" {
        return allow_csv.then(|| "text/plain".to_owned());
    }
    MIME_TABLE.iter().any(|(m, _)| *m == base).then_some(base)
}

/// Truncate to 255 characters keeping the extension; default `upload`.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    if name.chars().count() <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            let stem: String = stem.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Context resolved before the body is read.
pub struct UploadCtx {
    pub scopes: ChatScopes,
    pub chat: chats::Model,
    pub model: mini_chat_sdk::ModelCatalogEntry,
    pub kill_switches: mini_chat_sdk::KillSwitches,
    pub storage: StorageTarget,
}

/// Classification of an upload from its headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadKind {
    pub mime: String,
    pub kind: &'static str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub max_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct AttachmentDetail {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    pub error_code: Option<String>,
    pub img_thumbnail: Option<crate::domain::messages::Thumbnail>,
    pub created_at: OffsetDateTime,
}

impl AttachmentDetail {
    #[must_use]
    pub fn from_model(a: &attachments::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: a.status.clone(),
            kind: a.attachment_kind.clone(),
            error_code: if a.status == "failed" { a.error_code.clone() } else { None },
            img_thumbnail: crate::domain::messages::thumbnail_of(a),
            created_at: a.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup outbox payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

fn unavailable10() -> DomainError {
    DomainError::unavailable(10)
}

fn attachment_not_found(id: Uuid) -> DomainError {
    DomainError::not_found(Res::Attachment, id.to_string())
}

async fn set_status(
    state: &AppState,
    tenant_scope: &AccessScope,
    id: Uuid,
    status: &str,
    error_code: Option<&str>,
) -> DomainResult<()> {
    let conn = state.db.conn()?;
    attachments::Entity::update_many()
        .secure()
        .col_expr(attachments::Column::Status, Expr::value(status))
        .col_expr(attachments::Column::ErrorCode, Expr::value(error_code.map(str::to_owned)))
        .col_expr(attachments::Column::UpdatedAt, Expr::value(repo::now()))
        .filter(Condition::all().add(attachments::Column::Id.eq(id)))
        .scope_with(tenant_scope)
        .exec(&conn)
        .await?;
    Ok(())
}

impl AppState {
    /// Checks that run before the request body is read.
    ///
    /// # Errors
    /// 404 chat, 400 `INVALID_MODEL`, 500 on policy failure.
    pub async fn upload_precheck(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<UploadCtx> {
        let scopes = self.chat_scope(ctx, "upload_attachment", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = snap.find_model(&chat.model).cloned().ok_or_else(DomainError::invalid_model)?;
        let storage = self.resolver.storage_target(&model.provider_id, ctx.subject_tenant_id())?;
        Ok(UploadCtx {
            scopes,
            chat,
            model,
            kill_switches: snap.kill_switches,
            storage,
        })
    }

    /// Classify the upload from the part headers.
    ///
    /// # Errors
    /// Unsupported type, disabled images, unavailable code interpreter.
    pub fn classify_upload(&self, up: &UploadCtx, declared: &str, filename: &str) -> DomainResult<UploadKind> {
        let mime = resolve_mime(declared, filename, self.cfg.rag.allow_csv_upload).ok_or_else(|| {
            DomainError::field(Res::Attachment, "content_type", "UNSUPPORTED_CONTENT_TYPE", "unsupported file type")
        })?;
        let model_cap = u64::from(up.model.general_config.max_file_size_mb) * 1024 * 1024;
        if is_image(&mime) {
            if up.kill_switches.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            let cfg = u64::from(self.cfg.rag.uploaded_image_max_size_kb) * 1024;
            return Ok(UploadKind {
                mime,
                kind: "image",
                for_file_search: false,
                for_code_interpreter: false,
                max_bytes: if model_cap > 0 { cfg.min(model_cap) } else { cfg },
            });
        }
        let mut fs = mime != XLSX;
        let mut ci = mime == XLSX;
        if ci && (up.kill_switches.disable_code_interpreter || !up.model.general_config.tool_support.code_interpreter) {
            ci = false;
        }
        if !fs && !ci {
            return Err(DomainError::field(
                Res::Attachment,
                "file",
                "CODE_INTERPRETER_UNAVAILABLE",
                "code interpreter is not available for this chat",
            ));
        }
        if fs && mime == XLSX {
            fs = false;
        }
        let cfg = u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024;
        Ok(UploadKind {
            mime,
            kind: "document",
            for_file_search: fs,
            for_code_interpreter: ci,
            max_bytes: if model_cap > 0 { cfg.min(model_cap) } else { cfg },
        })
    }

    /// Upload once the body was read.
    #[allow(clippy::too_many_lines)]
    pub async fn upload_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        up: UploadCtx,
        filename: String,
        kind: UploadKind,
        data: Bytes,
        started: Instant,
    ) -> DomainResult<AttachmentDetail> {
        let tenant_scope = up.scopes.tenant.clone();
        let chat_id = up.chat.id;
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        // Per-chat limits (non-deleted, non-failed attachments).
        let conn = self.db.conn()?;
        let existing: Vec<attachments::Model> = repo::chat_attachments(&conn, &tenant_scope, chat_id)
            .await?
            .into_iter()
            .filter(|a| a.status != "failed")
            .collect();
        if kind.kind == "document" {
            let docs = existing.iter().filter(|a| a.attachment_kind == "document").count();
            if docs >= self.cfg.rag.max_documents_per_chat as usize {
                return Err(DomainError::ResourceExhausted {
                    res: Res::Attachment,
                    subject: "document_limit".to_owned(),
                    description: "maximum number of documents per chat reached".to_owned(),
                });
            }
        }
        let total: i64 = existing.iter().map(|a| a.size_bytes).sum();
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        if total.saturating_add(size) > max_total {
            return Err(DomainError::ResourceExhausted {
                res: Res::Attachment,
                subject: "storage_limit".to_owned(),
                description: "maximum total upload size per chat reached".to_owned(),
            });
        }
        let id = Uuid::new_v4();
        let now = repo::now();
        let am = attachments::ActiveModel {
            id: Set(id),
            tenant_id: Set(ctx.subject_tenant_id()),
            chat_id: Set(chat_id),
            uploaded_by_user_id: Set(ctx.subject_id()),
            filename: Set(filename.clone()),
            content_type: Set(kind.mime.clone()),
            size_bytes: Set(size),
            storage_backend: Set(up.storage.backend.clone()),
            provider_file_id: Set(None),
            status: Set("pending".to_owned()),
            error_code: Set(None),
            attachment_kind: Set(kind.kind.to_owned()),
            for_file_search: Set(kind.for_file_search),
            for_code_interpreter: Set(kind.for_code_interpreter),
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
        secure_insert::<attachments::Entity>(am, &tenant_scope, &conn).await?;
        drop(conn);

        let ext = extension(&filename).unwrap_or_else(|| "bin".to_owned());
        let provider_name = format!("{chat_id}_{id}.{ext}");
        let file_id = match self
            .storage
            .upload_file(&up.storage, &provider_name, &kind.mime, data.clone(), true)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "provider file upload failed");
                set_status(self, &tenant_scope, id, "failed", Some("upload_failed")).await?;
                return Err(unavailable10());
            }
        };
        let conn = self.db.conn()?;
        attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::Status, Expr::value("uploaded"))
            .col_expr(attachments::Column::ProviderFileId, Expr::value(Some(file_id.clone())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(repo::now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .scope_with(&tenant_scope)
            .exec(&conn)
            .await?;
        drop(conn);

        if kind.kind == "image" {
            let thumb = crate::infra::thumbnail::generate(&data, &self.cfg.thumbnail);
            let conn = self.db.conn()?;
            let mut q = attachments::Entity::update_many()
                .secure()
                .col_expr(attachments::Column::Status, Expr::value("ready"))
                .col_expr(attachments::Column::UpdatedAt, Expr::value(repo::now()));
            if let Some(t) = thumb {
                q = q
                    .col_expr(attachments::Column::ImgThumbnail, Expr::value(Some(t.bytes)))
                    .col_expr(attachments::Column::ImgThumbnailWidth, Expr::value(Some(i32::try_from(t.width).unwrap_or(0))))
                    .col_expr(attachments::Column::ImgThumbnailHeight, Expr::value(Some(i32::try_from(t.height).unwrap_or(0))));
            }
            q.filter(Condition::all().add(attachments::Column::Id.eq(id)))
                .scope_with(&tenant_scope)
                .exec(&conn)
                .await?;
        } else if kind.for_file_search {
            let vs = match self.chat_vector_store(&up, &tenant_scope).await {
                Ok(v) => v,
                Err(e) => {
                    set_status(self, &tenant_scope, id, "failed", Some("vector_store_failed")).await?;
                    self.delete_file_best_effort(&up.storage, &file_id);
                    return Err(e);
                }
            };
            let status = match self.storage.add_file_to_vector_store(&up.storage, &vs, &file_id, id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "adding file to vector store failed");
                    IndexStatus::Failed("add failed".to_owned())
                }
            };
            match self.wait_indexing(&up.storage, &vs, &file_id, status, started).await {
                IndexStatus::Completed => set_status(self, &tenant_scope, id, "ready", None).await?,
                IndexStatus::Failed(reason) => {
                    tracing::warn!(reason = %reason, "document indexing failed");
                    set_status(self, &tenant_scope, id, "failed", Some("indexing_failed")).await?;
                    self.delete_file_best_effort(&up.storage, &file_id);
                    return Err(unavailable10());
                }
                IndexStatus::InProgress => {
                    let state = Arc::clone(self);
                    let storage = up.storage.clone();
                    let tenant_id = ctx.subject_tenant_id();
                    tokio::spawn(async move {
                        state.background_indexing(tenant_id, chat_id, id, storage, vs, file_id).await;
                    });
                }
            }
        } else {
            set_status(self, &tenant_scope, id, "ready", None).await?;
        }
        let conn = self.db.conn()?;
        let row = repo::find_attachment(&conn, &tenant_scope, chat_id, id)
            .await?
            .ok_or_else(|| attachment_not_found(id))?;
        Ok(AttachmentDetail::from_model(&row))
    }

    fn delete_file_best_effort(&self, storage: &StorageTarget, file_id: &str) {
        let client = Arc::clone(&self.storage);
        let storage = storage.clone();
        let file_id = file_id.to_owned();
        tokio::spawn(async move {
            if let Err(e) = client.delete_file(&storage, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    async fn wait_indexing(
        &self,
        storage: &StorageTarget,
        vs: &str,
        file_id: &str,
        initial: IndexStatus,
        started: Instant,
    ) -> IndexStatus {
        let mut status = initial;
        let mut wait = Duration::from_millis(250);
        loop {
            match status {
                IndexStatus::InProgress => {}
                other => return other,
            }
            let elapsed = started.elapsed();
            if elapsed >= INDEX_DEADLINE {
                return IndexStatus::InProgress;
            }
            tokio::time::sleep(wait.min(INDEX_DEADLINE - elapsed)).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            if started.elapsed() >= INDEX_DEADLINE {
                return IndexStatus::InProgress;
            }
            let remaining = INDEX_DEADLINE.saturating_sub(started.elapsed());
            status = match tokio::time::timeout(remaining, self.storage.file_index_status(storage, vs, file_id)).await {
                Err(_) => return IndexStatus::InProgress,
                Ok(Ok(s)) => s,
                Ok(Err(StorageError::Transient(e))) => {
                    tracing::warn!(error = %e, "transient indexing status read error");
                    IndexStatus::InProgress
                }
                Ok(Err(StorageError::Failed(e))) => IndexStatus::Failed(e),
            };
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn background_indexing(
        self: Arc<Self>,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        storage: StorageTarget,
        vs: String,
        file_id: String,
    ) {
        let scope = AccessScope::for_tenant(tenant_id);
        let begun = Instant::now();
        let mut outcome = IndexStatus::InProgress;
        'rounds: while begun.elapsed() < BG_TOTAL {
            // Heartbeat: keep the reaper away while the row is still uploaded.
            let alive = match self.db.conn() {
                Ok(conn) => attachments::Entity::update_many()
                    .secure()
                    .col_expr(attachments::Column::UpdatedAt, Expr::value(repo::now()))
                    .filter(
                        Condition::all()
                            .add(attachments::Column::Id.eq(id))
                            .add(attachments::Column::Status.eq("uploaded"))
                            .add(attachments::Column::CleanupStatus.is_null())
                            .add(attachments::Column::DeletedAt.is_null()),
                    )
                    .scope_with(&scope)
                    .exec(&conn)
                    .await
                    .map(|r| r.rows_affected == 1)
                    .unwrap_or(false),
                Err(_) => false,
            };
            if !alive {
                return;
            }
            let round_start = Instant::now();
            let mut wait = Duration::from_millis(250);
            while round_start.elapsed() < BG_ROUND {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(Duration::from_secs(5));
                match self.storage.file_index_status(&storage, &vs, &file_id).await {
                    Ok(IndexStatus::InProgress) | Err(StorageError::Transient(_)) => {}
                    Ok(other) => {
                        outcome = other;
                        break 'rounds;
                    }
                    Err(StorageError::Failed(e)) => {
                        outcome = IndexStatus::Failed(e);
                        break 'rounds;
                    }
                }
            }
        }
        match outcome {
            IndexStatus::Completed => {
                for delay in [0_u64, 1, 2, 4] {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                    }
                    let Ok(conn) = self.db.conn() else { continue };
                    let res = attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::Status, Expr::value("ready"))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(repo::now()))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(id))
                                .add(attachments::Column::Status.eq("uploaded"))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .scope_with(&scope)
                        .exec(&conn)
                        .await;
                    if res.is_ok() {
                        return;
                    }
                }
            }
            IndexStatus::Failed(_) | IndexStatus::InProgress => {
                let outbox = self.outbox.clone();
                let storage_backend = storage.backend.clone();
                let fid = file_id.clone();
                let res = self.write_tx(move |tx| {
                        let outbox = outbox.clone();
                        let storage_backend = storage_backend.clone();
                        let fid = fid.clone();
                        Box::pin(async move {
                            let scope = AccessScope::for_tenant(tenant_id);
                            let now = repo::now();
                            let r = attachments::Entity::update_many()
                                .secure()
                                .col_expr(attachments::Column::Status, Expr::value("failed"))
                                .col_expr(attachments::Column::ErrorCode, Expr::value(Some("indexing_failed")))
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                                .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
                                .filter(
                                    Condition::all()
                                        .add(attachments::Column::Id.eq(id))
                                        .add(attachments::Column::Status.eq("uploaded"))
                                        .add(attachments::Column::CleanupStatus.is_null()),
                                )
                                .scope_with(&scope)
                                .exec(tx)
                                .await?;
                            let mut wakes = Wakes::default();
                            if r.rows_affected == 1 {
                                let p = AttachmentCleanupPayload {
                                    event_type: "attachment_indexing_failed".to_owned(),
                                    tenant_id,
                                    chat_id,
                                    attachment_id: id,
                                    provider_file_id: Some(fid),
                                    vector_store_id: None,
                                    storage_backend,
                                    attachment_kind: "document".to_owned(),
                                    deleted_at: now,
                                    secondary_ref: None,
                                };
                                wakes.push(outbox.enqueue(tx, Queue::AttachmentCleanup, tenant_id, &p).await?);
                            }
                            Ok(wakes)
                        })
                    })
                    .await;
                match res {
                    Ok(w) => w.fire(),
                    Err(e) => tracing::warn!(error = %e, "failed to record background indexing failure"),
                }
            }
        }
    }

    /// Get or create the chat's vector store (creation protocol, §3.7).
    async fn chat_vector_store(&self, up: &UploadCtx, tenant_scope: &AccessScope) -> DomainResult<String> {
        let tenant_id = up.chat.tenant_id;
        let chat_id = up.chat.id;
        for _attempt in 0..3 {
            let conn = self.db.conn()?;
            if let Some(row) = repo::find_vector_store(&conn, tenant_scope, tenant_id, chat_id).await? {
                if row.provider != up.storage.backend {
                    return Err(DomainError::already_exists(
                        Res::Attachment,
                        "provider_mismatch",
                        "the chat's vector store belongs to another provider backend",
                    ));
                }
                if let Some(v) = row.vector_store_id {
                    return Ok(v);
                }
                if (repo::now() - row.created_at).whole_seconds() > PLACEHOLDER_STALE_SECS {
                    repo::delete_vector_store_row(&conn, tenant_scope, row.id).await?;
                    continue;
                }
                drop(conn);
                return self.poll_vector_store(tenant_scope, tenant_id, chat_id).await;
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_stores::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(tenant_id),
                chat_id: Set(chat_id),
                vector_store_id: Set(None),
                provider: Set(up.storage.backend.clone()),
                file_count: Set(0),
                created_at: Set(repo::now()),
            };
            match chat_vector_stores::Entity::insert(am)
                .secure()
                .scope_unchecked(tenant_scope)?
                .exec(&conn)
                .await
            {
                Ok(_) => {}
                Err(e) if e.is_unique_violation() => {
                    drop(conn);
                    return self.poll_vector_store(tenant_scope, tenant_id, chat_id).await;
                }
                Err(e) => return Err(e.into()),
            }
            drop(conn);
            let vs = match self.storage.create_vector_store(&up.storage, chat_id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "vector store creation failed");
                    if let Ok(conn) = self.db.conn() {
                        let _ = repo::delete_vector_store_row(&conn, tenant_scope, row_id).await;
                    }
                    return Err(unavailable10());
                }
            };
            let conn = self.db.conn()?;
            let res = chat_vector_stores::Entity::update_many()
                .secure()
                .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(Some(vs.clone())))
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::Id.eq(row_id))
                        .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                )
                .scope_with(tenant_scope)
                .exec(&conn)
                .await?;
            if res.rows_affected == 1 {
                return Ok(vs);
            }
            let client = Arc::clone(&self.storage);
            let st = up.storage.clone();
            let vs_c = vs.clone();
            tokio::spawn(async move {
                let _ = client.delete_vector_store(&st, &vs_c).await;
            });
            drop(conn);
            return self.poll_vector_store(tenant_scope, tenant_id, chat_id).await;
        }
        Err(unavailable10())
    }

    async fn poll_vector_store(&self, tenant_scope: &AccessScope, tenant_id: Uuid, chat_id: Uuid) -> DomainResult<String> {
        let mut wait = Duration::from_millis(100);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(row) = repo::find_vector_store(&conn, tenant_scope, tenant_id, chat_id).await?
                && let Some(v) = row.vector_store_id
            {
                return Ok(v);
            }
        }
        Err(unavailable10())
    }

    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> DomainResult<AttachmentDetail> {
        let scopes = self.chat_scope(ctx, "read_attachment", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let a = repo::find_attachment(&conn, &scopes.tenant, chat_id, id)
            .await?
            .filter(|a| a.deleted_at.is_none() && a.uploaded_by_user_id == ctx.subject_id())
            .ok_or_else(|| attachment_not_found(id))?;
        Ok(AttachmentDetail::from_model(&a))
    }

    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> DomainResult<()> {
        let scopes = self.chat_scope(ctx, "delete_attachment", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let a = repo::find_attachment(&conn, &scopes.tenant, chat_id, id)
            .await?
            .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
            .ok_or_else(|| attachment_not_found(id))?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        if repo::attachment_referenced(&conn, &scopes.tenant, chat_id, id).await? {
            return Err(DomainError::already_exists(
                Res::Attachment,
                "attachment_locked",
                "the attachment is referenced by a message",
            ));
        }
        drop(conn);
        let outbox = self.outbox.clone();
        let tenant_scope = scopes.tenant.clone();
        let wakes = self
            .write_tx(move |tx| {
                let outbox = outbox.clone();
                let tenant_scope = tenant_scope.clone();
                let a = a.clone();
                Box::pin(async move {
                    let now = repo::now();
                    let res = attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(a.id))
                                .add(attachments::Column::DeletedAt.is_null()),
                        )
                        .scope_with(&tenant_scope)
                        .exec(tx)
                        .await?;
                    let mut wakes = Wakes::default();
                    if res.rows_affected == 0 {
                        return Ok(wakes);
                    }
                    let secondary_ref = match (&a.secondary_file_id, &a.secondary_provider_kind) {
                        (Some(f), Some(k)) if a.secondary_status == "uploaded" => Some(SecondaryRef {
                            file_id: f.clone(),
                            provider_kind: k.clone(),
                            upstream_alias: String::new(),
                        }),
                        _ => None,
                    };
                    let p = AttachmentCleanupPayload {
                        event_type: "attachment_deleted".to_owned(),
                        tenant_id: a.tenant_id,
                        chat_id: a.chat_id,
                        attachment_id: a.id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: now,
                        secondary_ref,
                    };
                    let w = outbox
                        .enqueue(tx, Queue::AttachmentCleanup, a.tenant_id, &p)
                        .await
                        .map_err(|e| match e {
                            DomainError::InvalidFormat { message, .. } => DomainError::internal(message),
                            other => other,
                        })?;
                    wakes.push(w);
                    Ok(wakes)
                })
            })
            .await?;
        wakes.fire();
        Ok(())
    }

    /// Upload reaper scan (B.9.5). Returns the number of reaped rows.
    pub async fn reap_uploads(&self) -> DomainResult<usize> {
        let stale = i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300);
        let cutoff = repo::now() - time::Duration::seconds(stale);
        let conn = self.db.conn()?;
        let rows = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::CleanupStatus.is_null()),
            )
            .all(&conn)
            .await?;
        drop(conn);
        let mut candidates: Vec<attachments::Model> = rows.into_iter().filter(|a| a.updated_at < cutoff).collect();
        candidates.sort_by_key(|a| a.updated_at);
        candidates.truncate(100);
        let mut reaped = 0;
        for a in candidates {
            let outbox = self.outbox.clone();
            let res = self.write_tx(move |tx| {
                    let outbox = outbox.clone();
                    let a = a.clone();
                    Box::pin(async move {
                        let scope = AccessScope::for_tenant(a.tenant_id);
                        let now = repo::now();
                        let mut q = attachments::Entity::update_many()
                            .secure()
                            .col_expr(attachments::Column::Status, Expr::value("failed"))
                            .col_expr(attachments::Column::ErrorCode, Expr::value(Some("upload_abandoned")))
                            .col_expr(attachments::Column::UpdatedAt, Expr::value(now));
                        if a.provider_file_id.is_some() {
                            q = q
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)));
                        }
                        let r = q
                            .filter(
                                Condition::all()
                                    .add(attachments::Column::Id.eq(a.id))
                                    .add(attachments::Column::Status.eq(a.status.clone()))
                                    .add(attachments::Column::DeletedAt.is_null())
                                    .add(attachments::Column::CleanupStatus.is_null()),
                            )
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                        let mut wakes = Wakes::default();
                        if r.rows_affected == 0 {
                            return Ok((false, wakes));
                        }
                        if a.provider_file_id.is_some() {
                            if a.secondary_file_id.is_some() {
                                tracing::warn!(secondary_file_id = ?a.secondary_file_id, "abandoned upload keeps its secondary copy");
                            }
                            let p = AttachmentCleanupPayload {
                                event_type: "attachment_upload_abandoned".to_owned(),
                                tenant_id: a.tenant_id,
                                chat_id: a.chat_id,
                                attachment_id: a.id,
                                provider_file_id: a.provider_file_id.clone(),
                                vector_store_id: None,
                                storage_backend: a.storage_backend.clone(),
                                attachment_kind: a.attachment_kind.clone(),
                                deleted_at: now,
                                secondary_ref: None,
                            };
                            wakes.push(outbox.enqueue(tx, Queue::AttachmentCleanup, a.tenant_id, &p).await?);
                        }
                        Ok((true, wakes))
                    })
                })
                .await;
            match res {
                Ok((true, w)) => {
                    w.fire();
                    reaped += 1;
                }
                Ok((false, _)) => {}
                Err(e) => tracing::warn!(error = %e, "upload reaper row failed"),
            }
        }
        Ok(reaped)
    }

    /// Remove the vector store row of a chat (chat cleanup).
    pub(crate) async fn drop_vector_store_row(&self, tenant_id: Uuid, row_id: Uuid) -> DomainResult<()> {
        let conn = self.db.conn()?;
        chat_vector_stores::Entity::delete_many()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_resolution() {
        assert_eq!(resolve_mime("application/pdf", "a.pdf", true).as_deref(), Some("application/pdf"));
        assert_eq!(resolve_mime("application/octet-stream", "a.PNG", true).as_deref(), Some("image/png"));
        assert_eq!(resolve_mime("application/octet-stream", "a.unknown", true), None);
        assert_eq!(resolve_mime("text/csv", "a.csv", true).as_deref(), Some("text/plain"));
        assert_eq!(resolve_mime("text/csv", "a.csv", false), None);
        assert_eq!(resolve_mime("text/plain; charset=utf-8", "a.txt", true).as_deref(), Some("text/plain"));
        assert_eq!(resolve_mime("application/x-msdownload", "a.exe", true), None);
        assert_eq!(resolve_mime("application/octet-stream", "sheet.xlsx", true).as_deref(), Some(XLSX));
    }

    #[test]
    fn filename_rules() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("")), "upload");
        let long = format!("{}.pdf", "a".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert!(n.ends_with(".pdf"));
    }
}
