//! Attachments: upload (provider file + vector store indexing + thumbnail), get, delete
//! (DESIGN "File Upload", "Attachment Deletion", ADR-0007).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use tokio::sync::OwnedSemaphorePermit;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::infra::db::WriteTransaction as _;
use crate::config::ProviderKind;
use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::service::{Svc, child_scope};
use crate::infra::db::entities::{attachments, chat_vector_stores, chats, message_attachments, messages};
use crate::infra::db::now;
use crate::infra::llm::resolver::StorageTarget;
use crate::infra::llm::storage::{IndexStatus, StorageClient};
use crate::infra::mime::{self, Classified, Kind};
use crate::infra::outbox::{AttachmentCleanupEvent, SecondaryRef};
use crate::infra::thumbnail;

/// Indexing deadline of the upload request, measured from the upload start.
pub const INDEX_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
pub const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
/// Background heartbeat round (refreshes `updated_at`).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
const _: () = assert!(BACKGROUND_ROUND.as_secs() * 2 <= 60, "heartbeat must be <= half the minimum stale_after_secs");

/// State gathered before the body is read.
pub struct UploadGate {
    /// Chat (scoped, owner).
    pub chat: chats::Model,
    /// Child scope of the chat.
    pub scope: AccessScope,
    /// Chat model entry (resolved without the enabled filter).
    pub entry: ModelCatalogEntry,
    /// Policy snapshot (kill switches).
    pub snapshot: PolicySnapshot,
    /// Upload start.
    pub started: Instant,
    _permit: OwnedSemaphorePermit,
}

/// A validated file part.
#[derive(Debug, Clone)]
pub struct PartPlan {
    /// Normalized filename.
    pub filename: String,
    /// Classification.
    pub class: Classified,
    /// Size limit in bytes.
    pub limit_bytes: u64,
}

fn mib(n: u32) -> u64 {
    u64::from(n) * 1024 * 1024
}

impl Svc {
    /// Authorizes, loads the chat, resolves its model and takes an upload slot (before the body is read).
    ///
    /// # Errors
    /// 404 chat, 403/503 PDP, `INVALID_MODEL`, 503 concurrency, 500 policy failure.
    pub async fn begin_upload(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<UploadGate, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id).await?;
        let (snapshot, resolved) = self.policy.resolve_chat_model(ctx.subject_id(), &chat.model).await?;
        let permit = self.upload_sem.clone().try_acquire_owned().map_err(|_| DomainError::UploadConcurrency)?;
        let scope = child_scope(&chat);
        Ok(UploadGate {
            chat,
            scope,
            entry: resolved.entry,
            snapshot,
            started: Instant::now(),
            _permit: permit,
        })
    }

    /// Validates the `file` part headers (MIME, kill switches, purposes, size limit).
    ///
    /// # Errors
    /// `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED` (images), code interpreter unavailable.
    pub fn plan_part(&self, gate: &UploadGate, content_type: &str, filename: Option<&str>) -> Result<PartPlan, DomainError> {
        let filename = mime::normalize_filename(filename);
        let ct = mime::effective_content_type(content_type, &filename);
        let mut class = mime::classify(&ct, self.cfg.rag.allow_csv_upload)
            .ok_or_else(|| DomainError::UnsupportedContentType(ct.clone()))?;
        let ks = &gate.snapshot.kill_switches;
        if class.kind == Kind::Image && ks.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        let ci_available = !ks.disable_code_interpreter && gate.entry.general_config.tool_support.code_interpreter;
        if class.for_code_interpreter && !ci_available {
            if !class.for_file_search {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
            class.for_code_interpreter = false;
        }
        let model_limit = mib(gate.entry.general_config.max_file_size_mb);
        let cfg_limit = match class.kind {
            Kind::Image => u64::from(self.cfg.rag.uploaded_image_max_size_kb) * 1024,
            Kind::Document => u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024,
        };
        Ok(PartPlan { filename, class, limit_bytes: cfg_limit.min(model_limit) })
    }

    /// Completes an upload with the received bytes.
    ///
    /// # Errors
    /// 409 provider mismatch, 429 limits, 503 storage failures.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "upload orchestration: limits transaction, provider upload, indexing and metrics in one flow"
    )]
    pub async fn complete_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        gate: UploadGate,
        plan: PartPlan,
        data: Bytes,
    ) -> Result<attachments::Model, DomainError> {
        let target = self.llm.resolver.storage_target(&gate.entry.provider_id, gate.chat.tenant_id)?;
        let conn = self.db.conn()?;
        if plan.class.for_file_search
            && let Some(vs) = crate::domain::repo::chat_vector_store(&conn, &gate.scope, gate.chat.id).await?
            && vs.provider != target.backend
        {
            return Err(DomainError::ProviderMismatch);
        }

        // Insert the pending row under the per-chat limits.
        let id = Uuid::now_v7();
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let ts = now();
        let am = attachments::ActiveModel {
            id: Set(id),
            tenant_id: Set(gate.chat.tenant_id),
            chat_id: Set(gate.chat.id),
            uploaded_by_user_id: Set(ctx.subject_id()),
            filename: Set(plan.filename.clone()),
            content_type: Set(plan.class.content_type.clone()),
            size_bytes: Set(size),
            storage_backend: Set(target.backend.clone()),
            provider_file_id: Set(None),
            status: Set("pending".to_owned()),
            error_code: Set(None),
            attachment_kind: Set(plan.class.kind.as_str().to_owned()),
            for_file_search: Set(plan.class.for_file_search),
            for_code_interpreter: Set(plan.class.for_code_interpreter),
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
            created_at: Set(ts),
            updated_at: Set(ts),
            deleted_at: Set(None),
            secondary_file_id: Set(None),
            secondary_status: Set("not_attempted".to_owned()),
            secondary_provider_kind: Set(None),
        };
        let max_docs = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = mib(self.cfg.rag.max_total_upload_mb_per_chat);
        let is_doc = plan.class.kind == Kind::Document;
        let scope = gate.scope.clone();
        let chat_id = gate.chat.id;
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let rows = attachments::Entity::find()
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::Status.ne("failed")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .all(tx)
                        .await?;
                    let docs = rows.iter().filter(|r| r.attachment_kind == "document").count() as u64;
                    let total: u64 = rows.iter().map(|r| u64::try_from(r.size_bytes).unwrap_or(0)).sum();
                    if is_doc && docs >= max_docs {
                        return Err(DomainError::DocumentLimit);
                    }
                    if total + u64::try_from(size).unwrap_or(u64::MAX) > max_total {
                        return Err(DomainError::StorageLimit);
                    }
                    Ok(secure_insert::<attachments::Entity>(am, &scope, tx).await?)
                })
            })
            .await;
        match res {
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        self.metrics.gauge_add("attachments_pending", 1);
        let labels = [("kind", plan.class.kind.as_str())];

        // Provider upload.
        let storage = StorageClient::new(&self.llm.gateway);
        let provider_name = format!("{}_{}.{}", gate.chat.id, id, plan.filename.rsplit_once('.').map_or("bin", |(_, e)| e));
        let file_id = match storage.upload_file(&target, &provider_name, &plan.class.content_type, data.clone()).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(attachment_id = %id, error = %e, "provider file upload failed");
                self.fail_row(&gate.scope, id, "upload_failed").await;
                self.metrics.gauge_add("attachments_pending", -1);
                self.metrics.inc("attachment_upload_total", &[("kind", plan.class.kind.as_str()), ("result", "failed")]);
                return Err(DomainError::StorageUnavailable(e.detail));
            }
        };
        let uploaded = self.set_uploaded(&gate.scope, id, &file_id).await;
        self.metrics.gauge_add("attachments_pending", -1);
        uploaded?;
        self.metrics.add("attachment_upload_bytes", data.len() as u64, &labels);

        match plan.class.kind {
            Kind::Image => {
                self.secondary_upload(&gate, id, &plan, data.clone()).await;
                let thumb = if data.len() <= self.cfg.thumbnail.max_decode_bytes {
                    let cfg = self.cfg.thumbnail.clone();
                    tokio::task::spawn_blocking(move || thumbnail::generate(&data, &cfg)).await.ok().flatten()
                } else {
                    None
                };
                self.set_ready(&gate.scope, id, thumb).await?;
            }
            Kind::Document if plan.class.for_file_search => {
                match self.index_document(&gate, &target, id, &file_id).await {
                    Ok(true) => self.set_ready(&gate.scope, id, None).await?,
                    Ok(false) => {
                        self.metrics.inc("attachment_upload_total", &[("kind", "document"), ("result", "ok")]);
                        return self.load_attachment(&gate.scope, id).await;
                    }
                    Err(code) => {
                        self.fail_row(&gate.scope, id, code).await;
                        let s = StorageClient::new(&self.llm.gateway);
                        if let Err(e) = s.delete_file(&target, &file_id).await {
                            tracing::warn!(attachment_id = %id, error = %e, "best-effort provider file delete failed");
                        }
                        self.metrics.inc("attachment_upload_total", &[("kind", "document"), ("result", "failed")]);
                        return Err(DomainError::StorageUnavailable(code.to_owned()));
                    }
                }
            }
            Kind::Document => self.set_ready(&gate.scope, id, None).await?,
        }
        self.metrics.inc("attachment_upload_total", &[("kind", plan.class.kind.as_str()), ("result", "ok")]);
        self.load_attachment(&gate.scope, id).await
    }

    async fn secondary_upload(&self, gate: &UploadGate, id: Uuid, plan: &PartPlan, data: Bytes) {
        let Some(entry) = self.llm.resolver.entries().get(&gate.entry.provider_id) else { return };
        if entry.kind != ProviderKind::AnthropicMessages {
            return;
        }
        let Ok(t) = self.llm.resolver.chat_target(&gate.entry.provider_id, gate.chat.tenant_id) else { return };
        let storage = StorageClient::new(&self.llm.gateway);
        let (status, file_id) = match storage.upload_anthropic_file(&t.alias, &plan.filename, &plan.class.content_type, data).await {
            Ok(f) => ("uploaded", Some(f)),
            Err(e) => {
                tracing::warn!(attachment_id = %id, error = %e, "secondary (Anthropic) upload failed");
                ("failed", None)
            }
        };
        if let Ok(conn) = self.db.conn()
            && let Err(e) = attachments::Entity::update_many()
                .secure()
                .col_expr(attachments::Column::SecondaryStatus, Expr::value(status))
                .col_expr(attachments::Column::SecondaryFileId, Expr::value(file_id))
                .col_expr(attachments::Column::SecondaryProviderKind, Expr::value(Some("anthropic".to_owned())))
                .filter(Condition::all().add(attachments::Column::Id.eq(id)))
                .scope_with(&gate.scope)
                .exec(&conn)
                .await
        {
            tracing::warn!(attachment_id = %id, error = %e, "recording secondary upload status failed");
        }
    }

    /// Gets or creates the chat vector store (INSERT-NULL claim, then CAS).
    async fn ensure_vector_store(&self, gate: &UploadGate, target: &StorageTarget) -> Result<String, DomainError> {
        let conn = self.db.conn()?;
        let deadline = gate.started + INDEX_DEADLINE;
        loop {
            if let Some(vs) = crate::domain::repo::chat_vector_store(&conn, &gate.scope, gate.chat.id).await? {
                if vs.provider != target.backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = vs.vector_store_id {
                    return Ok(id);
                }
                if Instant::now() >= deadline {
                    return Err(DomainError::StorageUnavailable("vector store creation in progress".into()));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let row_id = Uuid::now_v7();
            let am = chat_vector_stores::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(gate.chat.tenant_id),
                chat_id: Set(gate.chat.id),
                vector_store_id: Set(None),
                provider: Set(target.backend.clone()),
                file_count: Set(0),
                created_at: Set(now()),
            };
            let ins = chat_vector_stores::Entity::insert(am.clone())
                .secure()
                .scope_with_model(&gate.scope, &am)?
                .on_conflict_raw(
                    OnConflict::columns([chat_vector_stores::Column::TenantId, chat_vector_stores::Column::ChatId])
                        .do_nothing()
                        .to_owned(),
                )
                .exec(&conn)
                .await;
            match ins {
                Ok(_) => {}
                Err(ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => continue,
                Err(e) => return Err(e.into()),
            }
            // Winner: create the provider store and CAS the id in.
            let storage = StorageClient::new(&self.llm.gateway);
            match storage.create_vector_store(target, gate.chat.id).await {
                Ok(vs_id) => {
                    let r = chat_vector_stores::Entity::update_many()
                        .secure()
                        .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(Some(vs_id.clone())))
                        .filter(
                            Condition::all()
                                .add(chat_vector_stores::Column::Id.eq(row_id))
                                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                        )
                        .scope_with(&gate.scope)
                        .exec(&conn)
                        .await?;
                    if r.rows_affected == 1 {
                        return Ok(vs_id);
                    }
                    discard_vector_store(&storage, target, gate.chat.id, &vs_id).await;
                }
                Err(e) => {
                    tracing::warn!(chat_id = %gate.chat.id, error = %e, "vector store creation failed");
                    release_vector_store_claim(&conn, gate, row_id).await;
                    return Err(DomainError::StorageUnavailable(e.detail));
                }
            }
        }
    }

    /// Adds the file to the chat vector store and polls until the request deadline.
    /// `Ok(true)` = indexed, `Ok(false)` = still in progress (background task spawned),
    /// `Err(code)` = failed with the row error code.
    async fn index_document(
        self: &Arc<Self>,
        gate: &UploadGate,
        target: &StorageTarget,
        id: Uuid,
        file_id: &str,
    ) -> Result<bool, &'static str> {
        let vs_id = self.ensure_vector_store(gate, target).await.map_err(|_| "vector_store_failed")?;
        let storage = StorageClient::new(&self.llm.gateway);
        let mut status = storage.add_file(target, &vs_id, file_id, id).await.map_err(|e| {
            tracing::warn!(attachment_id = %id, error = %e, "vector store add file failed");
            "indexing_failed"
        })?;
        let deadline = gate.started + INDEX_DEADLINE;
        let mut wait = Duration::from_millis(250);
        while status == IndexStatus::InProgress {
            let now_i = Instant::now();
            if now_i >= deadline {
                break;
            }
            tokio::time::sleep(wait.min(deadline - now_i)).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match storage.file_status(target, &vs_id, file_id, left).await {
                Ok(s) => status = s,
                Err(e) if e.transient => {}
                Err(e) => {
                    tracing::warn!(attachment_id = %id, error = %e, "indexing status read failed");
                    return Err("indexing_failed");
                }
            }
        }
        match status {
            IndexStatus::Completed => Ok(true),
            IndexStatus::Failed => Err("indexing_failed"),
            IndexStatus::InProgress => {
                self.spawn_background_indexing(gate.scope.clone(), gate.chat.clone(), target.clone(), vs_id, file_id.to_owned(), id);
                Ok(false)
            }
        }
    }

    fn spawn_background_indexing(
        self: &Arc<Self>,
        scope: AccessScope,
        chat: chats::Model,
        target: StorageTarget,
        vs_id: String,
        file_id: String,
        id: Uuid,
    ) {
        let svc = self.clone();
        let cancel = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = svc.background_index(scope, chat, target, vs_id, file_id, id) => {}
            }
        });
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "background indexing orchestration: heartbeat, polling, ready retries and failure outbox"
    )]
    async fn background_index(
        &self,
        scope: AccessScope,
        chat: chats::Model,
        target: StorageTarget,
        vs_id: String,
        file_id: String,
        id: Uuid,
    ) {
        let started = Instant::now();
        let storage = StorageClient::new(&self.llm.gateway);
        let mut last_err: Option<String> = None;
        let outcome: &str = 'outer: loop {
            if started.elapsed() >= BACKGROUND_LIMIT {
                break 'outer "timeout";
            }
            // Heartbeat: refresh updated_at while still uploaded and not owned by cleanup.
            match self.heartbeat(&scope, id).await {
                Ok(true) => {}
                Ok(false) => {
                    self.metrics.inc("attachment_background_indexing_total", &[("result", "stopped")]);
                    return;
                }
                Err(e) => tracing::warn!(attachment_id = %id, error = %e, "indexing heartbeat failed"),
            }
            let round_end = Instant::now() + BACKGROUND_ROUND;
            let mut wait = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
                match storage.file_status(&target, &vs_id, &file_id, Duration::from_secs(30)).await {
                    Ok(IndexStatus::Completed) => break 'outer "completed",
                    Ok(IndexStatus::Failed) => break 'outer "failed",
                    Ok(IndexStatus::InProgress) => {}
                    Err(e) if e.transient => {
                        if last_err.is_none() {
                            tracing::warn!(attachment_id = %id, error = %e, "transient indexing status read error");
                        }
                        last_err = Some(e.detail);
                    }
                    Err(e) => {
                        last_err = Some(e.detail);
                        break 'outer "failed";
                    }
                }
                if started.elapsed() >= BACKGROUND_LIMIT {
                    break 'outer "timeout";
                }
            }
        };
        if outcome == "completed" {
            for (i, delay) in [0u64, 1, 2, 4].into_iter().enumerate() {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                match self.set_ready_if_uploaded(&scope, id).await {
                    Ok(_) => {
                        self.metrics.inc("attachment_background_indexing_total", &[("result", "ok")]);
                        return;
                    }
                    Err(e) if i == 3 => {
                        tracing::warn!(attachment_id = %id, error = %e, "setting attachment ready failed");
                    }
                    Err(_) => {}
                }
            }
            self.metrics.inc("attachment_background_indexing_total", &[("result", "set_ready_failed")]);
            return;
        }
        tracing::warn!(attachment_id = %id, outcome, last_error = ?last_err, "background indexing failed");
        let outbox = self.outbox.clone();
        let tenant = chat.tenant_id;
        let chat_id = chat.id;
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let r = attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::Status, Expr::value("failed"))
                        .col_expr(attachments::Column::ErrorCode, Expr::value(Some("indexing_failed".to_owned())))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(id))
                                .add(attachments::Column::Status.eq("uploaded"))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected != 1 {
                        return Ok(Wake::empty());
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_indexing_failed".to_owned(),
                        tenant_id: tenant,
                        chat_id,
                        attachment_id: id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: target.backend.clone(),
                        attachment_kind: "document".to_owned(),
                        deleted_at: ts,
                        secondary_ref: None,
                    };
                    outbox.attachment_cleanup(tx, &ev).await
                })
            })
            .await;
        match res {
            Ok(w) => w.fire(),
            Err(e) => tracing::warn!(attachment_id = %id, error = %e, "recording indexing failure failed"),
        }
        self.metrics.inc("attachment_background_indexing_total", &[("result", outcome)]);
    }

    async fn heartbeat(&self, scope: &AccessScope, id: Uuid) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let r = attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::Status.eq("uploaded"))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::CleanupStatus.is_null()),
            )
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(r.rows_affected == 1)
    }

    async fn set_ready_if_uploaded(&self, scope: &AccessScope, id: Uuid) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let r = attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::Status, Expr::value("ready"))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::Status.eq("uploaded"))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::CleanupStatus.is_null()),
            )
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(r.rows_affected == 1)
    }

    async fn set_uploaded(&self, scope: &AccessScope, id: Uuid, file_id: &str) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::Status, Expr::value("uploaded"))
            .col_expr(attachments::Column::ProviderFileId, Expr::value(Some(file_id.to_owned())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)).add(attachments::Column::Status.eq("pending")))
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn set_ready(&self, scope: &AccessScope, id: Uuid, thumb: Option<thumbnail::Thumbnail>) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let (data, w, h) = match thumb {
            Some(t) => (Some(t.data), i32::try_from(t.width).ok(), i32::try_from(t.height).ok()),
            None => (None, None, None),
        };
        attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::Status, Expr::value("ready"))
            .col_expr(attachments::Column::ImgThumbnail, Expr::value(data))
            .col_expr(attachments::Column::ImgThumbnailWidth, Expr::value(w))
            .col_expr(attachments::Column::ImgThumbnailHeight, Expr::value(h))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::Status.eq("uploaded"))
                    .add(attachments::Column::CleanupStatus.is_null()),
            )
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn fail_row(&self, scope: &AccessScope, id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let r = attachments::Entity::update_many()
            .secure()
            .col_expr(attachments::Column::Status, Expr::value("failed"))
            .col_expr(attachments::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::Status.is_in(["pending", "uploaded"])),
            )
            .scope_with(scope)
            .exec(&conn)
            .await;
        if let Err(e) = r {
            tracing::warn!(attachment_id = %id, error = %e, "marking attachment failed did not persist");
        }
    }

    async fn load_attachment(&self, scope: &AccessScope, id: Uuid) -> Result<attachments::Model, DomainError> {
        let conn = self.db.conn()?;
        attachments::Entity::find()
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound)
    }

    async fn owned_attachment(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<attachments::Model, DomainError> {
        attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::UploadedByUserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(scope)
            .one(runner)
            .await?
            .ok_or(DomainError::AttachmentNotFound)
    }

    /// `GET /chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404 chat / attachment, PDP errors.
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<attachments::Model, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let a = self.owned_attachment(&conn, ctx, &child_scope(&chat), chat_id, id).await?;
        if a.deleted_at.is_some() {
            return Err(DomainError::AttachmentNotFound);
        }
        Ok(a)
    }

    /// `DELETE /chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404, 409 `attachment_locked`, PDP errors.
    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::DELETE_ATTACHMENT, chat_id).await?;
        let scope = child_scope(&chat);
        let conn = self.db.conn()?;
        let a = self.owned_attachment(&conn, ctx, &scope, chat_id, id).await?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let links = message_attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat_id))
                    .add(message_attachments::Column::AttachmentId.eq(id)),
            )
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        if !links.is_empty() {
            let ids: Vec<Uuid> = links.iter().map(|l| l.message_id).collect();
            let live = messages::Entity::find()
                .filter(
                    Condition::all()
                        .add(messages::Column::Id.is_in(ids))
                        .add(messages::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?;
            if live.is_some() {
                return Err(DomainError::AttachmentLocked);
            }
        }
        let outbox = self.outbox.clone();
        let secondary_alias = a.secondary_file_id.as_ref().and_then(|_| {
            let (pid, _) = self
                .llm
                .resolver
                .entries()
                .iter()
                .find(|(_, e)| e.kind == ProviderKind::AnthropicMessages)?;
            self.llm.resolver.chat_target(pid, chat.tenant_id).ok().map(|t| t.alias)
        });
        let wake = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let r = attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(a.id))
                                .add(attachments::Column::DeletedAt.is_null()),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected != 1 {
                        return Ok(Wake::empty());
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_deleted".to_owned(),
                        tenant_id: a.tenant_id,
                        chat_id: a.chat_id,
                        attachment_id: a.id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: ts,
                        secondary_ref: match (&a.secondary_file_id, secondary_alias) {
                            (Some(f), Some(alias)) if a.secondary_status == "uploaded" => Some(SecondaryRef {
                                file_id: f.clone(),
                                provider_kind: "anthropic".to_owned(),
                                upstream_alias: alias,
                            }),
                            _ => None,
                        },
                    };
                    outbox.attachment_cleanup(tx, &ev).await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

/// Best-effort delete of a provider vector store that lost the CAS race.
async fn discard_vector_store(storage: &StorageClient<'_>, target: &StorageTarget, chat_id: Uuid, vs_id: &str) {
    if let Err(e) = storage.delete_vector_store(target, vs_id).await {
        tracing::warn!(chat_id = %chat_id, error = %e, "best-effort duplicate vector store delete failed");
    }
}

/// Best-effort release of an unfilled vector store claim row.
async fn release_vector_store_claim(conn: &impl DBRunner, gate: &UploadGate, row_id: Uuid) {
    if let Err(e) = chat_vector_stores::Entity::delete_many()
        .filter(
            Condition::all()
                .add(chat_vector_stores::Column::Id.eq(row_id))
                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
        )
        .secure()
        .scope_with(&gate.scope)
        .exec(conn)
        .await
    {
        tracing::warn!(chat_id = %gate.chat.id, error = %e, "releasing vector store claim failed");
    }
}

