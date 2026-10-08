//! Attachments: synchronous upload (provider file, vector store indexing,
//! thumbnail), background indexing, get and delete.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use tokio::sync::OwnedSemaphorePermit;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::Service;
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::{DisabledFeature, DomainError, DomainResult};
use crate::domain::events::{AttachmentCleanupEvent, PAYLOAD_ATTACHMENT_CLEANUP, SecondaryRef};
use crate::domain::mime::{self, MimeClass};
use crate::infra::llm::provider::StorageTarget;
use crate::infra::llm::storage::{IndexStatus, StorageError};
use crate::infra::outbox::{OutboxEnqueuer, fire};
use crate::infra::storage::entity::{attachment, chat, chat_vector_store, message_attachment};

/// Upload deadline for document indexing (from the upload start).
pub const INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
pub const BACKGROUND_INDEXING_LIMIT: Duration = Duration::from_secs(600);
/// Background indexing round (heartbeat interval).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
/// A NULL vector-store placeholder older than this is reclaimed.
pub const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);

const MIB: u64 = 1024 * 1024;

/// Pre-body upload context (chat, model, storage provider, slot).
pub struct UploadTarget {
    pub chat: chat::Model,
    pub scope: AccessScope,
    pub model: ModelCatalogEntry,
    pub snapshot: PolicySnapshot,
    pub storage: StorageTarget,
    pub anthropic_alias: Option<String>,
    pub started: Instant,
    _permit: OwnedSemaphorePermit,
}

/// MIME-derived routing of an upload.
#[derive(Debug, Clone)]
pub struct UploadPlan {
    pub mime: String,
    pub class: MimeClass,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub max_bytes: u64,
}

impl Service {
    /// Resolve everything an upload needs before its body is read.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`, `InvalidModel`, `PolicyResolution`,
    /// `UploadConcurrencyLimit`, `ProviderResolution`.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> DomainResult<UploadTarget> {
        let (chat, scope) = self
            .authorized_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id)
            .await?;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        let model = snapshot
            .find_model(&chat.model)
            .cloned()
            .ok_or_else(|| DomainError::invalid_model("chat model is no longer in the catalog"))?;
        let permit = Arc::clone(&self.upload_slots)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        let storage = self
            .providers
            .storage_target(&model.provider_id, chat.tenant_id)
            .map_err(|detail| DomainError::ProviderResolution { detail })?;
        let anthropic_alias = self
            .providers
            .anthropic_alias(&model.provider_id, chat.tenant_id);
        Ok(UploadTarget {
            chat,
            scope,
            model,
            snapshot,
            storage,
            anthropic_alias,
            started: Instant::now(),
            _permit: permit,
        })
    }

    /// Route an upload by its MIME type.
    ///
    /// # Errors
    /// `UnsupportedContentType`, `FeatureDisabled{images}`,
    /// `CodeInterpreterUnavailable`.
    pub fn plan_upload(&self, t: &UploadTarget, content_type: &str) -> DomainResult<UploadPlan> {
        let mut mime_type = mime::normalize(content_type);
        if mime_type == "text/csv" && self.cfg.rag.allow_csv_upload {
            "text/plain".clone_into(&mut mime_type);
        }
        let class =
            mime::classify(&mime_type).ok_or_else(|| DomainError::UnsupportedContentType {
                content_type: mime_type.clone(),
            })?;
        let ks = &t.snapshot.kill_switches;
        let model_cap = if t.model.general_config.max_file_size_mb > 0 {
            u64::from(t.model.general_config.max_file_size_mb) * MIB
        } else {
            u64::MAX
        };
        let (for_file_search, for_code_interpreter, rag_cap) = match class {
            MimeClass::Image => {
                if ks.disable_images {
                    return Err(DomainError::FeatureDisabled {
                        feature: DisabledFeature::Images,
                    });
                }
                (
                    false,
                    false,
                    u64::from(self.cfg.rag.uploaded_image_max_size_kb) * 1024,
                )
            }
            MimeClass::CodeFile => {
                if ks.disable_code_interpreter || !t.model.tool_support().code_interpreter {
                    return Err(DomainError::CodeInterpreterUnavailable);
                }
                (
                    false,
                    true,
                    u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024,
                )
            }
            MimeClass::Document => (
                true,
                false,
                u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024,
            ),
        };
        Ok(UploadPlan {
            mime: mime_type,
            class,
            for_file_search,
            for_code_interpreter,
            max_bytes: rag_cap.min(model_cap),
        })
    }

    async fn set_failed(&self, tenant_id: Uuid, id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let now = clock::now();
        let r = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("failed"))
            .col_expr(
                attachment::Column::ErrorCode,
                Expr::value(Some(code.to_owned())),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"])),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = r {
            tracing::error!(error = %e, "mini-chat: failed to mark attachment failed");
        }
    }

    async fn load_attachment(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<attachment::Model> {
        let conn = self.db.conn()?;
        attachment::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound { id })
    }

    /// Insert the row, upload to the provider, index / thumbnail and return
    /// the final row.
    ///
    /// # Errors
    /// `DocumentLimit` / `StorageLimit` (429), `ProviderMismatch` (409),
    /// `StorageUnavailable` (503).
    pub async fn store_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        t: UploadTarget,
        plan: UploadPlan,
        filename: String,
        data: Bytes,
    ) -> DomainResult<attachment::Model> {
        let kind = if plan.class == MimeClass::Image {
            "image"
        } else {
            "document"
        };
        #[allow(clippy::cast_precision_loss)]
        let bytes = data.len() as f64;
        self.metrics.attachments_pending.add(1, &[]);
        let r = self.store_upload_inner(ctx, t, plan, filename, data).await;
        self.metrics.attachments_pending.add(-1, &[]);
        let result = if r.is_ok() { "ok" } else { "error" };
        self.metrics.attachment_upload.add(
            1,
            &crate::infra::metrics::labels(&[("kind", kind), ("result", result)]),
        );
        self.metrics
            .attachment_upload_bytes
            .record(bytes, &crate::infra::metrics::labels(&[("kind", kind)]));
        r
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn store_upload_inner(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        t: UploadTarget,
        plan: UploadPlan,
        filename: String,
        data: Bytes,
    ) -> DomainResult<attachment::Model> {
        let tenant_id = t.chat.tenant_id;
        let chat_id = t.chat.id;
        let id = Uuid::new_v4();
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let is_image = plan.class == MimeClass::Image;
        let max_docs = i64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let row = {
            let scope = t.scope.clone();
            let filename = filename.clone();
            let plan = plan.clone();
            let backend = t.storage.backend_label.clone();
            let user_id = ctx.subject_id();
            self.tx(move |tx| {
                let scope = scope.clone();
                let filename = filename.clone();
                let plan = plan.clone();
                let backend = backend.clone();
                Box::pin(async move {
                    let live = attachment::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::Status.ne("failed")),
                        )
                        .all(tx)
                        .await?;
                    let docs = live.iter().filter(|a| a.attachment_kind == "document").count();
                    if !is_image && i64::try_from(docs).unwrap_or(i64::MAX) >= max_docs {
                        return Err(DomainError::DocumentLimit);
                    }
                    let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                    if total.saturating_add(size) > max_total {
                        return Err(DomainError::StorageLimit);
                    }
                    let now = clock::now();
                    let am = attachment::ActiveModel {
                        id: Set(id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        uploaded_by_user_id: Set(user_id),
                        filename: Set(filename),
                        content_type: Set(plan.mime.clone()),
                        size_bytes: Set(size),
                        storage_backend: Set(backend),
                        provider_file_id: Set(None),
                        status: Set("pending".to_owned()),
                        error_code: Set(None),
                        attachment_kind: Set(if is_image { "image" } else { "document" }.to_owned()),
                        for_file_search: Set(plan.for_file_search),
                        for_code_interpreter: Set(plan.for_code_interpreter),
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
                    Ok(secure_insert::<attachment::Entity>(am, &scope, tx).await?)
                })
            })
            .await?
        };

        // Provider upload.
        let ext = mime::extension(&filename).map_or_else(String::new, |e| format!(".{e}"));
        let provider_name = format!("{chat_id}_{id}{ext}");
        let file_id = match self
            .storage
            .upload_file(&t.storage, &provider_name, &plan.mime, data.clone())
            .await
        {
            Ok(f) => f,
            Err(e) => {
                self.set_failed(tenant_id, id, "upload_failed").await;
                return Err(DomainError::StorageUnavailable {
                    detail: e.to_string(),
                });
            }
        };
        {
            let conn = self.db.conn()?;
            attachment::Entity::update_many()
                .col_expr(attachment::Column::Status, Expr::value("uploaded"))
                .col_expr(
                    attachment::Column::ProviderFileId,
                    Expr::value(Some(file_id.clone())),
                )
                .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
                .filter(Condition::all().add(attachment::Column::Id.eq(id)))
                .secure()
                .scope_with(&t.scope)
                .exec(&conn)
                .await?;
        }

        // Secondary Anthropic copy of images.
        if is_image
            && let Some(alias) = &t.anthropic_alias
            && data.len() <= self.cfg.thumbnail.max_decode_bytes
        {
            let res = self
                .storage
                .upload_anthropic_file(alias, &provider_name, &plan.mime, data.clone())
                .await;
            let conn = self.db.conn()?;
            let (status, sid) = match res {
                Ok(sid) => ("uploaded", Some(sid)),
                Err(e) => {
                    tracing::warn!(error = %e, "mini-chat: secondary Anthropic upload failed");
                    ("failed", None)
                }
            };
            attachment::Entity::update_many()
                .col_expr(attachment::Column::SecondaryStatus, Expr::value(status))
                .col_expr(attachment::Column::SecondaryFileId, Expr::value(sid))
                .col_expr(
                    attachment::Column::SecondaryProviderKind,
                    Expr::value(Some("anthropic")),
                )
                .filter(Condition::all().add(attachment::Column::Id.eq(id)))
                .secure()
                .scope_with(&t.scope)
                .exec(&conn)
                .await?;
        }

        match plan.class {
            MimeClass::Image => {
                let thumb = super::thumbnail::make_thumbnail(&data, &self.cfg.thumbnail);
                let conn = self.db.conn()?;
                let (bytes, w, h) = match thumb {
                    Some(th) => (
                        Some(th.bytes),
                        Some(i32::try_from(th.width).unwrap_or_default()),
                        Some(i32::try_from(th.height).unwrap_or_default()),
                    ),
                    None => (None, None, None),
                };
                attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value("ready"))
                    .col_expr(attachment::Column::ImgThumbnail, Expr::value(bytes))
                    .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(w))
                    .col_expr(attachment::Column::ImgThumbnailHeight, Expr::value(h))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
                    .filter(Condition::all().add(attachment::Column::Id.eq(id)))
                    .secure()
                    .scope_with(&t.scope)
                    .exec(&conn)
                    .await?;
            }
            MimeClass::CodeFile => {
                self.set_ready(tenant_id, id).await?;
            }
            MimeClass::Document => {
                let vs = match self
                    .vector_store_for_chat(&t.scope, tenant_id, chat_id, &t.storage)
                    .await
                {
                    Ok(vs) => vs,
                    Err(e) => {
                        if matches!(e, DomainError::ProviderMismatch) {
                            self.set_failed(tenant_id, id, "vector_store_failed").await;
                            self.spawn_delete_file(t.storage.clone(), file_id.clone());
                            return Err(e);
                        }
                        self.set_failed(tenant_id, id, "vector_store_failed").await;
                        self.spawn_delete_file(t.storage.clone(), file_id.clone());
                        return Err(match e {
                            DomainError::StorageUnavailable { .. } => e,
                            other => DomainError::StorageUnavailable {
                                detail: other.to_string(),
                            },
                        });
                    }
                };
                let status = self
                    .storage
                    .add_file_to_vector_store(&t.storage, &vs, &file_id, &id.to_string())
                    .await;
                let mut status = match status {
                    Ok(s) => s,
                    Err(e) => {
                        self.set_failed(tenant_id, id, "indexing_failed").await;
                        self.spawn_delete_file(t.storage.clone(), file_id.clone());
                        return Err(DomainError::StorageUnavailable {
                            detail: e.to_string(),
                        });
                    }
                };
                let deadline = t.started + INDEXING_DEADLINE;
                let mut wait = Duration::from_millis(250);
                loop {
                    match status {
                        IndexStatus::Completed => {
                            self.set_ready(tenant_id, id).await?;
                            break;
                        }
                        IndexStatus::Failed(s) => {
                            self.set_failed(tenant_id, id, "indexing_failed").await;
                            self.spawn_delete_file(t.storage.clone(), file_id.clone());
                            return Err(DomainError::StorageUnavailable {
                                detail: format!("indexing status {s}"),
                            });
                        }
                        IndexStatus::InProgress => {}
                    }
                    let now = Instant::now();
                    if now + wait >= deadline {
                        // Still in progress at the deadline: finish in the
                        // background.
                        self.spawn_background_indexing(
                            tenant_id,
                            chat_id,
                            id,
                            t.storage.clone(),
                            vs.clone(),
                            file_id.clone(),
                        );
                        break;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(2));
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let read = tokio::time::timeout(
                        remaining,
                        self.storage
                            .vector_store_file_status(&t.storage, &vs, &file_id),
                    )
                    .await;
                    status = match read {
                        Err(_) => IndexStatus::InProgress,
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) if e.transient => IndexStatus::InProgress,
                        Ok(Err(e)) => IndexStatus::Failed(format!("status read error: {e}")),
                    };
                }
            }
        }
        let _ = row;
        self.load_attachment(tenant_id, id).await
    }

    async fn set_ready(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let conn = self.db.conn()?;
        let res = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("ready"))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.eq("uploaded"))
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        Ok(res.rows_affected > 0)
    }

    fn spawn_delete_file(&self, target: StorageTarget, file_id: String) {
        let storage = self.storage.clone();
        tokio::spawn(async move {
            if let Err(e) = storage.delete_file(&target, &file_id).await {
                tracing::warn!(error = %e, "mini-chat: best-effort provider file delete failed");
            }
        });
    }

    /// Get or create the chat vector store (creation protocol with a NULL
    /// placeholder and a CAS).
    async fn vector_store_for_chat(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        target: &StorageTarget,
    ) -> DomainResult<String> {
        for _round in 0..2 {
            let conn = self.db.conn()?;
            let existing = chat_vector_store::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
                .one(&conn)
                .await?;
            if let Some(row) = &existing {
                if row.provider != target.backend_label {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = &row.vector_store_id {
                    return Ok(vs.clone());
                }
                let age = clock::now() - row.created_at;
                if age > time::Duration::try_from(STALE_PLACEHOLDER).unwrap_or(time::Duration::MAX)
                {
                    chat_vector_store::Entity::delete_many()
                        .filter(
                            Condition::all()
                                .add(chat_vector_store::Column::Id.eq(row.id))
                                .add(chat_vector_store::Column::VectorStoreId.is_null()),
                        )
                        .secure()
                        .scope_with(scope)
                        .exec(&conn)
                        .await?;
                    continue;
                }
                return self.poll_vector_store(scope, chat_id).await;
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_store::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(tenant_id),
                chat_id: Set(chat_id),
                vector_store_id: Set(None),
                provider: Set(target.backend_label.clone()),
                file_count: Set(0),
                created_at: Set(clock::now()),
            };
            match secure_insert::<chat_vector_store::Entity>(am, scope, &conn).await {
                Ok(_) => {}
                Err(e) if e.is_unique_violation() => {
                    return self.poll_vector_store(scope, chat_id).await;
                }
                Err(e) => return Err(e.into()),
            }
            let created = self
                .storage
                .create_vector_store(target, &format!("chat-{chat_id}"))
                .await;
            let vs = match created {
                Ok(vs) => vs,
                Err(e) => {
                    if let Err(del) = chat_vector_store::Entity::delete_many()
                        .filter(Condition::all().add(chat_vector_store::Column::Id.eq(row_id)))
                        .secure()
                        .scope_with(scope)
                        .exec(&conn)
                        .await
                    {
                        tracing::warn!(error = %del, "mini-chat: placeholder cleanup failed");
                    }
                    return Err(DomainError::StorageUnavailable {
                        detail: e.to_string(),
                    });
                }
            };
            let res = chat_vector_store::Entity::update_many()
                .col_expr(
                    chat_vector_store::Column::VectorStoreId,
                    Expr::value(Some(vs.clone())),
                )
                .filter(
                    Condition::all()
                        .add(chat_vector_store::Column::Id.eq(row_id))
                        .add(chat_vector_store::Column::VectorStoreId.is_null()),
                )
                .secure()
                .scope_with(scope)
                .exec(&conn)
                .await?;
            if res.rows_affected == 1 {
                return Ok(vs);
            }
            // Placeholder reclaimed meanwhile: drop our store, use the chat's.
            let storage = self.storage.clone();
            let t = target.clone();
            tokio::spawn(async move {
                if let Err(e) = storage.delete_vector_store(&t, &vs).await {
                    tracing::warn!(error = %e, "mini-chat: duplicate vector store not deleted");
                }
            });
            return self.poll_vector_store(scope, chat_id).await;
        }
        Err(DomainError::StorageUnavailable {
            detail: "vector store creation did not converge".to_owned(),
        })
    }

    async fn poll_vector_store(&self, scope: &AccessScope, chat_id: Uuid) -> DomainResult<String> {
        let mut wait = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(vs) = chat_vector_store::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
                .one(&conn)
                .await?
                .and_then(|r| r.vector_store_id)
            {
                return Ok(vs);
            }
        }
        Err(DomainError::StorageUnavailable {
            detail: "vector store creation by a concurrent upload did not finish".to_owned(),
        })
    }

    fn spawn_background_indexing(
        self: &Arc<Self>,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        target: StorageTarget,
        vs: String,
        file_id: String,
    ) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let shutdown = svc.shutdown.clone();
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = svc.background_indexing(tenant_id, chat_id, id, &target, &vs, &file_id) => {}
            }
        });
    }

    #[allow(clippy::cognitive_complexity)]
    async fn background_indexing(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        target: &StorageTarget,
        vs: &str,
        file_id: &str,
    ) {
        let start = Instant::now();
        let scope = AccessScope::for_tenant(tenant_id);
        let mut failure: Option<String> = None;
        'rounds: while start.elapsed() < BACKGROUND_INDEXING_LIMIT {
            // Heartbeat (stops when the row is gone / no longer ours).
            let Ok(conn) = self.db.conn() else { return };
            let hb = attachment::Entity::update_many()
                .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
                .filter(
                    Condition::all()
                        .add(attachment::Column::Id.eq(id))
                        .add(attachment::Column::Status.eq("uploaded"))
                        .add(attachment::Column::CleanupStatus.is_null())
                        .add(attachment::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await;
            match hb {
                Ok(r) if r.rows_affected == 0 => return,
                Err(_) => return,
                Ok(_) => {}
            }
            let round_end = Instant::now() + BACKGROUND_ROUND;
            let mut wait = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
                match self
                    .storage
                    .vector_store_file_status(target, vs, file_id)
                    .await
                {
                    Ok(IndexStatus::Completed) => {
                        for delay in [1u64, 2, 4, 0] {
                            match self.set_ready(tenant_id, id).await {
                                Ok(_) => {
                                    self.metrics.attachment_background_indexing.add(
                                        1,
                                        &crate::infra::metrics::labels(&[("result", "ready")]),
                                    );
                                    return;
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "mini-chat: set ready failed");
                                    if delay == 0 {
                                        self.metrics.attachment_background_indexing.add(
                                            1,
                                            &crate::infra::metrics::labels(&[(
                                                "result",
                                                "set_ready_failed",
                                            )]),
                                        );
                                        return;
                                    }
                                    tokio::time::sleep(Duration::from_secs(delay)).await;
                                }
                            }
                        }
                        return;
                    }
                    Ok(IndexStatus::InProgress)
                    | Err(StorageError {
                        transient: true, ..
                    }) => {}
                    Ok(IndexStatus::Failed(s)) => {
                        failure = Some(format!("indexing status {s}"));
                        break 'rounds;
                    }
                    Err(e) => {
                        failure = Some(e.to_string());
                        break 'rounds;
                    }
                }
                if start.elapsed() >= BACKGROUND_INDEXING_LIMIT {
                    break 'rounds;
                }
            }
        }
        tracing::warn!(attachment_id = %id, reason = ?failure, "mini-chat: background indexing failed");
        self.metrics.attachment_background_indexing.add(
            1,
            &crate::infra::metrics::labels(&[(
                "result",
                if failure.is_some() {
                    "failed"
                } else {
                    "timeout"
                },
            )]),
        );
        let outbox = Arc::clone(&self.outbox);
        let file_id = file_id.to_owned();
        let label = target.backend_label.clone();
        let res = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let file_id = file_id.clone();
                let label = label.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(tenant_id);
                    let now = clock::now();
                    let r = attachment::Entity::update_many()
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value(Some("indexing_failed")),
                        )
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending")),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(id))
                                .add(attachment::Column::Status.eq("uploaded"))
                                .add(attachment::Column::CleanupStatus.is_null())
                                .add(attachment::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(Vec::new());
                    }
                    let event = AttachmentCleanupEvent {
                        event_type: "attachment_indexing_failed".to_owned(),
                        tenant_id,
                        chat_id,
                        attachment_id: id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: label,
                        attachment_kind: "document".to_owned(),
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Ok(vec![
                        outbox
                            .enqueue_json(
                                tx,
                                &outbox.queues.cleanup_queue_name,
                                tenant_id,
                                PAYLOAD_ATTACHMENT_CLEANUP,
                                &event,
                            )
                            .await?,
                    ])
                })
            })
            .await;
        match res {
            Ok(w) => fire(w),
            Err(e) => {
                tracing::error!(error = %e, "mini-chat: background indexing failure not recorded");
            }
        }
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`, `AttachmentNotFound`.
    pub async fn get_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<attachment::Model> {
        let (_, scope) = self
            .authorized_chat(ctx, actions::READ_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::UploadedByUserId.eq(ctx.subject_id())),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound { id: attachment_id })
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// Authz errors, not found, `AttachmentLocked`.
    pub async fn delete_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<()> {
        let (chat, scope) = self
            .authorized_chat(ctx, actions::DELETE_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let att = attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::UploadedByUserId.eq(ctx.subject_id())),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound { id: attachment_id })?;
        if att.deleted_at.is_some() {
            return Ok(());
        }
        let linked = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
            )
            .count(&conn)
            .await?;
        if linked > 0 {
            return Err(DomainError::AttachmentLocked);
        }
        let secondary = match (&att.secondary_file_id, att.secondary_status.as_str()) {
            (Some(fid), "uploaded") => self
                .providers
                .anthropic_alias(
                    &self
                        .snapshot(ctx.subject_id())
                        .await
                        .ok()
                        .and_then(|s| s.find_model(&chat.model).map(|m| m.provider_id.clone()))
                        .unwrap_or_default(),
                    chat.tenant_id,
                )
                .map(|alias| SecondaryRef {
                    file_id: fid.clone(),
                    provider_kind: "anthropic".to_owned(),
                    upstream_alias: alias,
                }),
            _ => None,
        };
        let outbox = Arc::clone(&self.outbox);
        let res = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let scope = scope.clone();
                let att = att.clone();
                let secondary = secondary.clone();
                Box::pin(async move {
                    soft_delete_attachment(tx, &outbox, &scope, &att, secondary).await
                })
            })
            .await;
        match res {
            Ok(w) => {
                fire(w);
                Ok(())
            }
            Err(DomainError::OutboxPayloadTooLarge { detail }) => {
                Err(DomainError::internal(detail))
            }
            Err(e) => Err(e),
        }
    }
}

async fn soft_delete_attachment(
    tx: &toolkit_db::DbTx<'_>,
    outbox: &OutboxEnqueuer,
    scope: &AccessScope,
    att: &attachment::Model,
    secondary: Option<SecondaryRef>,
) -> DomainResult<Vec<toolkit_db::outbox::Wake>> {
    let now = clock::now();
    let r = attachment::Entity::update_many()
        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some("pending")),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(att.id))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    if r.rows_affected == 0 {
        return Ok(Vec::new());
    }
    let event = AttachmentCleanupEvent {
        event_type: "attachment_deleted".to_owned(),
        tenant_id: att.tenant_id,
        chat_id: att.chat_id,
        attachment_id: att.id,
        provider_file_id: att.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: att.storage_backend.clone(),
        attachment_kind: att.attachment_kind.clone(),
        deleted_at: now,
        secondary_ref: secondary,
    };
    Ok(vec![
        outbox
            .enqueue_json(
                tx,
                &outbox.queues.cleanup_queue_name,
                att.tenant_id,
                PAYLOAD_ATTACHMENT_CLEANUP,
                &event,
            )
            .await?,
    ])
}
