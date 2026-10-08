//! Attachment upload (synchronous, with background indexing past the request
//! deadline), status and deletion (DESIGN §3.3, §3.6, ADR-0007).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::Stream;
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::error::{DomainError, Res};
use super::messages::{ThumbnailView, thumbnail_of};
use super::mime;
use super::service::Services;
use crate::config::ProviderKind;
use crate::infra::db::entities::{attachment, chat};
use crate::infra::db::repo::{attachments, vector_stores};
use crate::infra::db::{now_ts, tenant_scope, with_retry};
use crate::infra::llm::registry::StorageTarget;
use crate::infra::llm::storage::{IndexStatus, StorageError};
use crate::infra::outbox::{self, AttachmentCleanupMsg, Queue, SecondaryRef};

/// Indexing deadline of an upload request.
const INDEX_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
/// Heartbeat round of the background indexing task.
const HEARTBEAT_ROUND: Duration = Duration::from_secs(20);
/// A NULL vector-store placeholder older than this is stale.
const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);

/// Attachment details (`AttachmentDetail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetailView {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    pub error_code: Option<String>,
    pub img_thumbnail: Option<ThumbnailView>,
    pub created_at: OffsetDateTime,
}

impl AttachmentDetailView {
    #[must_use]
    pub fn from_model(a: &attachment::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: a.status.clone(),
            kind: a.attachment_kind.clone(),
            error_code: if a.status == attachments::STATUS_FAILED {
                a.error_code.clone()
            } else {
                None
            },
            img_thumbnail: thumbnail_of(a),
            created_at: a.created_at,
        }
    }
}

fn multipart_err(
    field: &'static str,
    reason: &'static str,
    desc: impl Into<String>,
) -> DomainError {
    DomainError::invalid(Res::Attachment, field, reason, desc)
}

impl Services {
    /// `POST /chats/{id}/attachments`.
    #[allow(clippy::too_many_lines)]
    pub async fn upload_attachment<S, O, E>(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        content_type: Option<String>,
        body: S,
    ) -> Result<AttachmentDetailView, DomainError>
    where
        S: Stream<Item = Result<O, E>> + Send + 'static,
        O: Into<Bytes> + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
    {
        let Ok(_permit) = self.upload_slots.clone().try_acquire_owned() else {
            return Err(DomainError::Unavailable {
                retry_after_secs: 5,
                diagnostic: "upload concurrency limit reached".to_owned(),
            });
        };
        let started = Instant::now();
        let chat = self.load_chat(ctx, "upload_attachment", chat_id).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = snapshot
            .find_model(&chat.model)
            .cloned()
            .ok_or_else(|| DomainError::invalid_model(Res::Chat))?;
        let storage = self
            .providers
            .storage_for(&model.provider_id, chat.tenant_id)
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "no storage provider for provider '{}'",
                    model.provider_id
                ))
            })?;

        // Multipart parsing (streaming).
        let ct = content_type.unwrap_or_default();
        let boundary = multer::parse_boundary(&ct).map_err(|_| {
            multipart_err(
                "content_type",
                "BOUNDARY_REQUIRED",
                "multipart boundary is required",
            )
        })?;
        let mut mp = multer::Multipart::new(body, boundary);
        let mut field = loop {
            match mp.next_field().await {
                Ok(Some(f)) if f.name() == Some("file") => break f,
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(multipart_err(
                        "file",
                        "MISSING_FILE",
                        "the 'file' field is required",
                    ));
                }
                Err(e) => {
                    return Err(multipart_err("multipart", "MULTIPART_ERROR", e.to_string()));
                }
            }
        };
        let filename = mime::normalize_filename(field.file_name());
        let Some(part_type) = field.content_type().map(ToString::to_string) else {
            return Err(multipart_err(
                "content_type",
                "MISSING_CONTENT_TYPE",
                "the 'file' part has no content type",
            ));
        };
        let Some(mime_type) = mime::resolve(&part_type, &filename, self.cfg.rag.allow_csv_upload)
        else {
            return Err(multipart_err(
                "content_type",
                "UNSUPPORTED_CONTENT_TYPE",
                format!("unsupported content type '{part_type}'"),
            ));
        };
        let is_image = mime::is_image(&mime_type);
        let ks = snapshot.kill_switches;
        if is_image && ks.disable_images {
            return Err(DomainError::precondition(
                Res::Attachment,
                "images",
                "FEATURE_DISABLED",
                "images are disabled",
            ));
        }
        let (for_fs, mut for_ci) = mime::purposes(&mime_type);
        if for_ci
            && (ks.disable_code_interpreter || !model.general_config.tool_support.code_interpreter)
        {
            for_ci = false;
            if !for_fs {
                return Err(multipart_err(
                    "file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                    "code interpreter is unavailable for this chat",
                ));
            }
        }
        let kb_limit = if is_image {
            self.cfg.rag.uploaded_image_max_size_kb
        } else {
            self.cfg.rag.uploaded_file_max_size_kb
        };
        let mut limit = u64::from(kb_limit) * 1024;
        if model.general_config.max_file_size_mb > 0 {
            limit = limit.min(u64::from(model.general_config.max_file_size_mb) * 1024 * 1024);
        }
        let mut buf = BytesMut::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if (buf.len() + chunk.len()) as u64 > limit {
                        return Err(DomainError::out_of_range(
                            Res::Attachment,
                            "content_length",
                            "FILE_TOO_LARGE",
                            format!("the file exceeds the {limit}-byte limit"),
                        ));
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(multipart_err("multipart", "MULTIPART_ERROR", e.to_string()));
                }
            }
        }
        drop(mp);
        let data = buf.freeze();
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let kind = if is_image {
            attachments::KIND_IMAGE
        } else {
            attachments::KIND_DOCUMENT
        };
        let scope = tenant_scope(chat.tenant_id);

        // Vector-store backend must match the chat's store.
        if for_fs && !is_image {
            let conn = self.db.conn()?;
            if let Some(row) = vector_stores::find(&conn, &scope, chat.tenant_id, chat.id).await?
                && row.provider != storage.backend_label
            {
                return Err(DomainError::AlreadyExists {
                    res: Res::Attachment,
                    name: "provider_mismatch".to_owned(),
                    detail: "The chat's documents are stored with another provider".to_owned(),
                });
            }
        }

        // Per-chat limits and the pending row.
        let att_id = Uuid::new_v4();
        let secondary_alias = self
            .providers
            .anthropic_alias(&model.provider_id, chat.tenant_id);
        let row = attachment::Model {
            id: att_id,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            uploaded_by_user_id: ctx.subject_id(),
            filename: filename.clone(),
            content_type: mime_type.clone(),
            size_bytes: size,
            storage_backend: storage.backend_label.clone(),
            provider_file_id: None,
            status: attachments::STATUS_PENDING.to_owned(),
            error_code: None,
            attachment_kind: kind.to_owned(),
            for_file_search: for_fs,
            for_code_interpreter: for_ci,
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
            created_at: now_ts(),
            updated_at: now_ts(),
            deleted_at: None,
            secondary_file_id: None,
            secondary_status: if is_image && secondary_alias.is_some() {
                "pending".to_owned()
            } else {
                "not_attempted".to_owned()
            },
            secondary_provider_kind: (is_image && secondary_alias.is_some())
                .then(|| "anthropic".to_owned()),
        };
        let max_docs = i64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        with_retry(|| {
            let scope = scope.clone();
            let row = row.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let active =
                        attachments::active_for_limits_list(tx, &scope, row.chat_id).await?;
                    if row.attachment_kind == attachments::KIND_DOCUMENT {
                        let docs = active
                            .iter()
                            .filter(|a| a.attachment_kind == attachments::KIND_DOCUMENT)
                            .count();
                        if i64::try_from(docs).unwrap_or(i64::MAX) >= max_docs {
                            return Err(DomainError::ChatLimit {
                                subject: "document_limit",
                                description: format!("At most {max_docs} documents per chat"),
                            });
                        }
                    }
                    let total: i64 = active.iter().map(|a| a.size_bytes).sum();
                    if total.saturating_add(row.size_bytes) > max_total {
                        return Err(DomainError::ChatLimit {
                            subject: "storage_limit",
                            description: "The chat storage limit is exceeded".to_owned(),
                        });
                    }
                    attachments::insert(tx, &scope, &row).await
                })
            })
        })
        .await?;
        self.metrics.gauge_add("attachments_pending", 1);
        let res = self
            .process_upload(
                &chat,
                &scope,
                &storage,
                &row,
                data,
                started,
                secondary_alias,
            )
            .await;
        self.metrics.gauge_add("attachments_pending", -1);
        let kind_label = kind;
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "attachment_upload_bytes",
            size as f64,
            &[("kind", kind_label)],
        );
        self.metrics.inc(
            "attachment_upload",
            &[
                ("kind", kind_label),
                ("result", if res.is_ok() { "ok" } else { "error" }),
            ],
        );
        res
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::cognitive_complexity
    )]
    async fn process_upload(
        self: &Arc<Self>,
        chat: &chat::Model,
        scope: &AccessScope,
        storage: &StorageTarget,
        row: &attachment::Model,
        data: Bytes,
        started: Instant,
        secondary_alias: Option<String>,
    ) -> Result<AttachmentDetailView, DomainError> {
        let ext = row.filename.rsplit_once('.').map_or("bin", |(_, e)| e);
        let provider_name = format!("{}_{}.{ext}", chat.id, row.id);
        let file_id = match self
            .llm
            .upload_file(storage, &provider_name, &row.content_type, data.clone())
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.fail_row(scope, row.id, "upload_failed").await;
                return Err(DomainError::storage_unavailable(e.to_string()));
            }
        };
        let conn = self.db.conn()?;
        attachments::mark_uploaded(&conn, scope, row.id, &file_id, now_ts()).await?;

        if let Some(alias) = &secondary_alias
            && row.attachment_kind == attachments::KIND_IMAGE
        {
            if data.len() <= self.cfg.thumbnail.max_decode_bytes {
                match self
                    .llm
                    .anthropic_upload(alias, &row.filename, &row.content_type, data.clone())
                    .await
                {
                    Ok(sid) => {
                        attachments::set_secondary(
                            &conn,
                            scope,
                            row.id,
                            Some(&sid),
                            "uploaded",
                            now_ts(),
                        )
                        .await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "secondary image upload failed");
                        attachments::set_secondary(&conn, scope, row.id, None, "failed", now_ts())
                            .await?;
                    }
                }
            } else {
                attachments::set_secondary(&conn, scope, row.id, None, "not_attempted", now_ts())
                    .await?;
            }
        }

        if row.attachment_kind == attachments::KIND_DOCUMENT && row.for_file_search {
            let vs = match self.ensure_vector_store(chat, scope, storage).await {
                Ok(v) => v,
                Err(e) => {
                    self.fail_row(scope, row.id, "vector_store_failed").await;
                    self.best_effort_delete(storage, &file_id);
                    return Err(e);
                }
            };
            let status = self
                .llm
                .add_vector_store_file(storage, &vs, &file_id, &row.id.to_string())
                .await;
            let status = match status {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "vector store file add failed");
                    self.fail_row(scope, row.id, "indexing_failed").await;
                    self.best_effort_delete(storage, &file_id);
                    return Err(DomainError::storage_unavailable(e.to_string()));
                }
            };
            match self
                .wait_indexed(storage, &vs, &file_id, status, started)
                .await
            {
                Indexing::Completed => {
                    attachments::mark_ready(&conn, scope, row.id, None, now_ts()).await?;
                }
                Indexing::Failed(reason) => {
                    tracing::warn!(%reason, "document indexing failed");
                    self.fail_row(scope, row.id, "indexing_failed").await;
                    self.best_effort_delete(storage, &file_id);
                    return Err(DomainError::storage_unavailable(reason));
                }
                Indexing::Pending => {
                    let svc = self.clone();
                    let storage = storage.clone();
                    let scope = scope.clone();
                    let row = row.clone();
                    tokio::spawn(async move {
                        svc.background_indexing(scope, storage, row, vs, file_id)
                            .await;
                    });
                }
            }
        } else if row.attachment_kind == attachments::KIND_IMAGE {
            let cfg = self.cfg.thumbnail.clone();
            let thumb =
                tokio::task::spawn_blocking(move || crate::infra::thumbnail::generate(&data, &cfg))
                    .await
                    .ok()
                    .flatten()
                    .map(|t| attachments::Thumbnail {
                        bytes: t.bytes,
                        width: i32::try_from(t.width).unwrap_or(0),
                        height: i32::try_from(t.height).unwrap_or(0),
                    });
            attachments::mark_ready(&conn, scope, row.id, thumb, now_ts()).await?;
        } else {
            attachments::mark_ready(&conn, scope, row.id, None, now_ts()).await?;
        }
        let a = attachments::find_in_chat(&conn, scope, chat.id, row.id, true)
            .await?
            .ok_or_else(|| DomainError::internal("attachment vanished"))?;
        Ok(AttachmentDetailView::from_model(&a))
    }

    async fn fail_row(&self, scope: &AccessScope, id: Uuid, code: &str) {
        if let Ok(conn) = self.db.conn()
            && let Err(e) = attachments::mark_failed(&conn, scope, id, code, false, now_ts()).await
        {
            tracing::warn!(error = %e, "failed to mark attachment failed");
        }
    }

    fn best_effort_delete(&self, storage: &StorageTarget, file_id: &str) {
        let llm = self.llm.clone();
        let storage = storage.clone();
        let file_id = file_id.to_owned();
        tokio::spawn(async move {
            if let Err(e) = llm.delete_file(&storage, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    async fn wait_indexed(
        &self,
        storage: &StorageTarget,
        vs: &str,
        file_id: &str,
        first: IndexStatus,
        started: Instant,
    ) -> Indexing {
        let mut status = first;
        let mut wait = Duration::from_millis(250);
        loop {
            match status {
                IndexStatus::Completed => return Indexing::Completed,
                IndexStatus::Failed(s) => return Indexing::Failed(format!("indexing status {s}")),
                IndexStatus::InProgress => {}
            }
            let elapsed = started.elapsed();
            if elapsed >= INDEX_DEADLINE {
                return Indexing::Pending;
            }
            tokio::time::sleep(wait.min(INDEX_DEADLINE.saturating_sub(elapsed))).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            let remaining = INDEX_DEADLINE.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Indexing::Pending;
            }
            match tokio::time::timeout(
                remaining,
                self.llm.get_vector_store_file(storage, vs, file_id),
            )
            .await
            {
                Err(_) => return Indexing::Pending,
                Ok(Ok(s)) => status = s,
                Ok(Err(StorageError::Transient(e))) => {
                    tracing::debug!(error = %e, "transient indexing status read error");
                }
                Ok(Err(e)) => return Indexing::Failed(e.to_string()),
            }
        }
    }

    /// Background indexing of a document returned as `uploaded`.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn background_indexing(
        self: Arc<Self>,
        scope: AccessScope,
        storage: StorageTarget,
        row: attachment::Model,
        vs: String,
        file_id: String,
    ) {
        let started = Instant::now();
        let mut last_transient: Option<String> = None;
        let outcome = 'outer: loop {
            if started.elapsed() >= BACKGROUND_LIMIT {
                break 'outer BgOutcome::Timeout;
            }
            let alive = match self.db.conn() {
                Ok(conn) => attachments::heartbeat_uploaded(&conn, &scope, row.id, now_ts())
                    .await
                    .unwrap_or(true),
                Err(_) => true,
            };
            if !alive {
                break 'outer BgOutcome::Stop;
            }
            let round_end = Instant::now() + HEARTBEAT_ROUND;
            let mut wait = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::select! {
                    () = self.shutdown.cancelled() => break 'outer BgOutcome::Stop,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(Duration::from_secs(5));
                match self
                    .llm
                    .get_vector_store_file(&storage, &vs, &file_id)
                    .await
                {
                    Ok(IndexStatus::Completed) => break 'outer BgOutcome::Ready,
                    Ok(IndexStatus::Failed(s)) => break 'outer BgOutcome::Failed(s),
                    Ok(IndexStatus::InProgress) => {}
                    Err(StorageError::Transient(e)) => {
                        if last_transient.is_none() {
                            tracing::warn!(error = %e, "transient indexing status error");
                        }
                        last_transient = Some(e);
                    }
                    Err(e) => break 'outer BgOutcome::Failed(e.to_string()),
                }
                if started.elapsed() >= BACKGROUND_LIMIT {
                    break 'outer BgOutcome::Timeout;
                }
            }
        };
        match outcome {
            BgOutcome::Stop => {}
            BgOutcome::Ready => {
                let mut ok = false;
                for delay in [0u64, 1, 2, 4] {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                    }
                    if let Ok(conn) = self.db.conn()
                        && attachments::mark_ready(&conn, &scope, row.id, None, now_ts())
                            .await
                            .is_ok()
                    {
                        ok = true;
                        break;
                    }
                }
                self.metrics.inc(
                    "attachment_background_indexing",
                    &[("result", if ok { "ready" } else { "set_ready_failed" })],
                );
            }
            BgOutcome::Failed(_) | BgOutcome::Timeout => {
                let timeout = matches!(outcome, BgOutcome::Timeout);
                if let BgOutcome::Failed(reason) = &outcome {
                    tracing::warn!(attachment_id = %row.id, %reason, "background indexing failed");
                }
                if timeout {
                    tracing::warn!(attachment_id = %row.id, last_error = ?last_transient, "background indexing timed out");
                }
                let ob = self.outbox.clone();
                let r = row.clone();
                let sc = scope.clone();
                let fid = file_id.clone();
                let res = self
                    .db
                    .transaction(move |tx| {
                        Box::pin(async move {
                            let now = now_ts();
                            if !attachments::mark_failed(
                                tx,
                                &sc,
                                r.id,
                                "indexing_failed",
                                true,
                                now,
                            )
                            .await?
                            {
                                return Ok(None);
                            }
                            let msg = AttachmentCleanupMsg {
                                event_type: "attachment_indexing_failed".to_owned(),
                                tenant_id: r.tenant_id,
                                chat_id: r.chat_id,
                                attachment_id: r.id,
                                provider_file_id: Some(fid),
                                vector_store_id: None,
                                storage_backend: r.storage_backend.clone(),
                                attachment_kind: r.attachment_kind.clone(),
                                deleted_at: now,
                                secondary_ref: None,
                            };
                            Ok(Some(
                                ob.enqueue(tx, Queue::AttachmentCleanup, r.tenant_id, &msg)
                                    .await?,
                            ))
                        })
                    })
                    .await;
                if let Ok(Some(w)) = res {
                    w.fire();
                }
                self.metrics.inc(
                    "attachment_background_indexing",
                    &[("result", if timeout { "timeout" } else { "failed" })],
                );
            }
        }
    }

    /// Get or create the chat's vector store (one per chat, DESIGN §3.7).
    async fn ensure_vector_store(
        &self,
        chat: &chat::Model,
        scope: &AccessScope,
        storage: &StorageTarget,
    ) -> Result<String, DomainError> {
        for _ in 0..3 {
            let conn = self.db.conn()?;
            match vector_stores::find(&conn, scope, chat.tenant_id, chat.id).await? {
                Some(row) if row.vector_store_id.is_some() => {
                    if row.provider != storage.backend_label {
                        return Err(DomainError::AlreadyExists {
                            res: Res::Attachment,
                            name: "provider_mismatch".to_owned(),
                            detail: "The chat's documents are stored with another provider"
                                .to_owned(),
                        });
                    }
                    return Ok(row.vector_store_id.unwrap_or_default());
                }
                Some(row) => {
                    let age = OffsetDateTime::now_utc() - row.created_at;
                    if age
                        > time::Duration::seconds(
                            i64::try_from(STALE_PLACEHOLDER.as_secs()).unwrap_or(i64::MAX),
                        )
                    {
                        vector_stores::delete_row(&conn, scope, row.id).await?;
                        continue;
                    }
                    return self.poll_vector_store(chat, scope).await;
                }
                None => {}
            }
            let row_id = Uuid::new_v4();
            match vector_stores::insert_placeholder(
                &conn,
                scope,
                row_id,
                chat.tenant_id,
                chat.id,
                &storage.backend_label,
                now_ts(),
            )
            .await
            {
                Ok(()) => {}
                Err(e) if e.is_unique_violation() => {
                    return self.poll_vector_store(chat, scope).await;
                }
                Err(e) => return Err(e),
            }
            let vs = match self
                .llm
                .create_vector_store(storage, &format!("chat-{}", chat.id))
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    if let Err(del) = vector_stores::delete_row(&conn, scope, row_id).await {
                        tracing::warn!(error = %del, "failed to remove the vector store placeholder");
                    }
                    return Err(DomainError::storage_unavailable(e.to_string()));
                }
            };
            if vector_stores::cas_set_id(&conn, scope, row_id, &vs).await? {
                return Ok(vs);
            }
            let llm = self.llm.clone();
            let st = storage.clone();
            let vs2 = vs.clone();
            tokio::spawn(async move {
                if let Err(e) = llm.delete_vector_store(&st, &vs2).await {
                    tracing::warn!(error = %e, "failed to delete a duplicate vector store");
                }
            });
            return self.poll_vector_store(chat, scope).await;
        }
        Err(DomainError::storage_unavailable(
            "vector store creation did not converge",
        ))
    }

    async fn poll_vector_store(
        &self,
        chat: &chat::Model,
        scope: &AccessScope,
    ) -> Result<String, DomainError> {
        let mut wait = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(row) = vector_stores::find(&conn, scope, chat.tenant_id, chat.id).await?
                && let Some(id) = row.vector_store_id
            {
                return Ok(id);
            }
        }
        Err(DomainError::storage_unavailable(
            "vector store is still being created",
        ))
    }

    /// `GET /chats/{id}/attachments/{attachment_id}`.
    pub async fn get_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetailView, DomainError> {
        let chat = self.load_chat(ctx, "read_attachment", chat_id).await?;
        let conn = self.db.conn()?;
        let a = attachments::find_in_chat(
            &conn,
            &tenant_scope(chat.tenant_id),
            chat_id,
            attachment_id,
            false,
        )
        .await?
        .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
        .ok_or_else(|| DomainError::not_found(Res::Attachment, attachment_id.to_string()))?;
        Ok(AttachmentDetailView::from_model(&a))
    }

    /// `DELETE /chats/{id}/attachments/{attachment_id}` (idempotent).
    pub async fn delete_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let chat = self.load_chat(ctx, "delete_attachment", chat_id).await?;
        let scope = tenant_scope(chat.tenant_id);
        let a = {
            let conn = self.db.conn()?;
            attachments::find_in_chat(&conn, &scope, chat_id, attachment_id, true)
                .await?
                .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
                .ok_or_else(|| DomainError::not_found(Res::Attachment, attachment_id.to_string()))?
        };
        if a.deleted_at.is_some() {
            return Ok(());
        }
        {
            let conn = self.db.conn()?;
            if attachments::is_referenced(&conn, &scope, chat_id, attachment_id).await? {
                return Err(DomainError::AlreadyExists {
                    res: Res::Attachment,
                    name: "attachment_locked".to_owned(),
                    detail: "The attachment is referenced by a message".to_owned(),
                });
            }
        }
        let secondary_alias = self
            .policy
            .current_snapshot(ctx.subject_id())
            .await
            .ok()
            .and_then(|s| s.find_model(&chat.model).map(|m| m.provider_id.clone()))
            .and_then(|pid| {
                self.providers
                    .get(&pid)
                    .filter(|e| e.kind == ProviderKind::AnthropicMessages)
                    .map(|_| pid)
            })
            .and_then(|pid| self.providers.anthropic_alias(&pid, chat.tenant_id));
        let wake = with_retry(|| {
            let scope = scope.clone();
            let a = a.clone();
            let ob = self.outbox.clone();
            let secondary_alias = secondary_alias.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    if !attachments::soft_delete(tx, &scope, a.id, now).await? {
                        return Ok(None);
                    }
                    let secondary_ref = match (&a.secondary_file_id, secondary_alias) {
                        (Some(fid), Some(alias)) => Some(SecondaryRef {
                            file_id: fid.clone(),
                            provider_kind: "anthropic".to_owned(),
                            upstream_alias: alias,
                        }),
                        _ => None,
                    };
                    let msg = AttachmentCleanupMsg {
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
                    let w = ob
                        .enqueue(tx, Queue::AttachmentCleanup, a.tenant_id, &msg)
                        .await
                        .map_err(|e| match e {
                            DomainError::InvalidFormat(m) => DomainError::internal(m),
                            other => other,
                        })?;
                    Ok(Some(w))
                })
            })
        })
        .await?;
        if let Some(w) = wake {
            outbox::fire(vec![w]);
        }
        Ok(())
    }
}

enum Indexing {
    Completed,
    Failed(String),
    Pending,
}

enum BgOutcome {
    Ready,
    Failed(String),
    Timeout,
    Stop,
}
