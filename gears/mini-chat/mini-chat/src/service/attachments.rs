//! Attachments: upload (provider Files API, chat vector store, indexing
//! wait, thumbnails), background indexing, get and delete (DESIGN §3.3,
//! "File Upload", "Attachment Deletion").

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Set};
use time::OffsetDateTime;
use tokio::sync::OwnedSemaphorePermit;
use toolkit_db::secure::{
    AccessScope, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::outbox::{AttachmentCleanupEvent, EnqueueError, SecondaryRef};
use crate::config::ProviderKind;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::mime;
use crate::infra::db::entity::{attachment, chat, chat_vector_store, message_attachment};
use crate::infra::llm::ResolvedStorage;
use crate::infra::llm::storage::VsFileStatus;
use crate::infra::repo::{self, now_utc};
use crate::infra::thumbnail;

/// Synchronous indexing wait, measured from the start of the upload.
pub const UPLOAD_INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing: total time budget.
const BG_INDEXING_LIMIT: Duration = Duration::from_secs(600);
/// Background indexing: heartbeat round.
const BG_ROUND: Duration = Duration::from_secs(20);
/// Placeholder vector-store rows older than this are reclaimed.
const STALE_PLACEHOLDER_SECS: i64 = 120;

// The heartbeat must stay well below the reaper's minimum staleness (60 s).
const _: () = assert!(BG_ROUND.as_secs() * 2 <= 60);

/// Everything resolved before the body is read.
pub struct UploadTarget {
    pub chat: chat::Model,
    pub scope: AccessScope,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
    pub storage: ResolvedStorage,
    /// Upstream alias of the Anthropic provider serving the chat's model.
    pub anthropic_alias: Option<String>,
    pub started: Instant,
    pub permit: OwnedSemaphorePermit,
}

/// MIME classification of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
pub struct FileClass {
    pub content_type: String,
    pub is_image: bool,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

impl UploadTarget {
    /// Effective per-file limit in bytes:
    /// `min(gear config by kind, model max_file_size_mb)`.
    #[must_use]
    pub fn limit_bytes(&self, cfg: &crate::config::RagConfig, is_image: bool) -> u64 {
        let gear_kb = if is_image {
            cfg.uploaded_image_max_size_kb
        } else {
            cfg.uploaded_file_max_size_kb
        };
        let gear = u64::from(gear_kb) * 1024;
        let model = u64::from(self.model.general_config.max_file_size_mb) * 1024 * 1024;
        if model == 0 { gear } else { gear.min(model) }
    }
}

fn unsupported_type(ct: &str) -> DomainError {
    DomainError::invalid(
        Res::Attachment,
        "content_type",
        "UNSUPPORTED_CONTENT_TYPE",
        format!("unsupported file type '{ct}'"),
    )
}

/// Resolve the MIME type of the `file` part (`application/octet-stream` is
/// inferred from the extension).
///
/// # Errors
/// 400 `UNSUPPORTED_CONTENT_TYPE`.
pub fn classify(
    part_content_type: &str,
    filename: &str,
    allow_csv: bool,
) -> DomainResult<FileClass> {
    let mut ct = mime::base_mime(part_content_type);
    if ct == "application/octet-stream"
        && let Some(m) = mime::mime_from_filename(filename)
    {
        m.clone_into(&mut ct);
    }
    let validated = mime::validate_mime(&ct, allow_csv).ok_or_else(|| unsupported_type(&ct))?;
    let is_image = mime::is_image(&validated);
    let (fs, ci) = mime::purposes(&validated);
    Ok(FileClass {
        content_type: validated,
        is_image,
        for_file_search: fs,
        for_code_interpreter: ci,
    })
}

fn storage_unavailable() -> DomainError {
    DomainError::unavailable(10)
}

fn too_large() -> DomainError {
    DomainError::out_of_range(
        Res::Attachment,
        "content_length",
        "FILE_TOO_LARGE",
        "the file exceeds the upload size limit",
    )
}

/// Error returned when the streamed body exceeds the limit.
#[must_use]
pub fn file_too_large() -> DomainError {
    too_large()
}

impl AppState {
    /// Authorization, chat lookup and model / storage resolution before the
    /// body is read.
    ///
    /// # Errors
    /// 403/503 (PDP), 404 chat, 400 `INVALID_MODEL`, 500 policy failure,
    /// 503 concurrency limit.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> DomainResult<UploadTarget> {
        let scopes = authz::chat_scopes(
            &self.enforcer,
            ctx,
            actions::UPLOAD_ATTACHMENT,
            Some(chat_id),
        )
        .await?;
        let conn = self.conn()?;
        let chat = repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let snapshot = self.chat_model_snapshot(ctx, &chat).await?;
        let model = snapshot
            .find(&chat.model)
            .cloned()
            .ok_or_else(|| DomainError::invalid_model("the chat's model is no longer available"))?;
        let storage = self
            .llm
            .registry
            .storage_for(&model.provider_id, Some(ctx.subject_tenant_id()))
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "no storage provider configured for provider '{}'",
                    model.provider_id
                ))
            })?;
        let anthropic_alias = self
            .llm
            .registry
            .resolve(&model.provider_id, Some(ctx.subject_tenant_id()))
            .filter(|p| p.kind == ProviderKind::AnthropicMessages)
            .map(|p| p.alias);
        let permit = Arc::clone(&self.upload_sem)
            .try_acquire_owned()
            .map_err(|_| DomainError::unavailable(5))?;
        Ok(UploadTarget {
            chat,
            scope: scopes.tenant,
            model,
            kill_switches: snapshot.kill_switches,
            storage,
            anthropic_alias,
            started: Instant::now(),
            permit,
        })
    }

    /// Kill-switch and capability filtering of the purposes.
    ///
    /// # Errors
    /// 400 `FEATURE_DISABLED` (images) or `CODE_INTERPRETER_UNAVAILABLE`.
    #[allow(
        clippy::unused_self,
        reason = "kept as a method for call-site symmetry"
    )]
    pub fn filter_purposes(&self, t: &UploadTarget, class: &mut FileClass) -> DomainResult<()> {
        if class.is_image && t.kill_switches.disable_images {
            return Err(DomainError::FailedPrecondition {
                res: Res::Attachment,
                subject: "images".into(),
                type_: "FEATURE_DISABLED".into(),
                description: "images is disabled by the operator".into(),
            });
        }
        let ci_available = !t.kill_switches.disable_code_interpreter
            && t.model.general_config.tool_support.code_interpreter;
        if class.for_code_interpreter && !ci_available {
            if !class.for_file_search {
                return Err(DomainError::invalid(
                    Res::Attachment,
                    "file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                    "code interpreter is not available for this chat",
                ));
            }
            class.for_code_interpreter = false;
        }
        Ok(())
    }

    /// Check the vector-store backend of the chat (409 `provider_mismatch`).
    async fn check_provider_match(&self, t: &UploadTarget) -> DomainResult<()> {
        let conn = self.conn()?;
        let row = chat_vector_store::Entity::find()
            .secure()
            .scope_with(&t.scope)
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(t.chat.id)))
            .one(&conn)
            .await?;
        if let Some(r) = row
            && r.provider != t.storage.backend_label
        {
            return Err(DomainError::already_exists(
                Res::Attachment,
                "provider_mismatch",
                "the chat's documents are stored with another provider backend",
            ));
        }
        Ok(())
    }

    /// Insert the `pending` row, enforcing the per-chat limits.
    async fn insert_pending(
        &self,
        ctx: &SecurityContext,
        t: &UploadTarget,
        id: Uuid,
        filename: &str,
        class: &FileClass,
        size: i64,
    ) -> DomainResult<attachment::Model> {
        let max_docs = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_bytes = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let now = now_utc();
        let am = attachment::ActiveModel {
            id: Set(id),
            tenant_id: Set(ctx.subject_tenant_id()),
            chat_id: Set(t.chat.id),
            uploaded_by_user_id: Set(ctx.subject_id()),
            filename: Set(filename.to_owned()),
            content_type: Set(class.content_type.clone()),
            size_bytes: Set(size),
            storage_backend: Set(t.storage.backend_label.clone()),
            provider_file_id: Set(None),
            status: Set("pending".to_owned()),
            error_code: Set(None),
            attachment_kind: Set(if class.is_image { "image" } else { "document" }.to_owned()),
            for_file_search: Set(class.for_file_search),
            for_code_interpreter: Set(class.for_code_interpreter),
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
        let scope = t.scope.clone();
        let chat_id = t.chat.id;
        let is_image = class.is_image;
        self.write_tx(move |tx| {
            let am = am.clone();
            let scope = scope.clone();
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
                if !is_image {
                    let docs = live
                        .iter()
                        .filter(|a| a.attachment_kind == "document")
                        .count() as u64;
                    if docs >= max_docs {
                        return Err(DomainError::ResourceExhausted {
                            res: Res::Attachment,
                            subject: "document_limit".into(),
                            description: "the chat's document limit is reached".into(),
                        });
                    }
                }
                let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                if total.saturating_add(size) > max_bytes {
                    return Err(DomainError::ResourceExhausted {
                        res: Res::Attachment,
                        subject: "storage_limit".into(),
                        description: "the chat's storage limit is reached".into(),
                    });
                }
                Ok(secure_insert::<attachment::Entity>(am, &scope, tx).await?)
            })
        })
        .await
    }

    async fn update_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        f: impl FnOnce(
            toolkit_db::secure::SecureUpdateMany<attachment::Entity, toolkit_db::secure::Scoped>,
        ) -> toolkit_db::secure::SecureUpdateMany<
            attachment::Entity,
            toolkit_db::secure::Scoped,
        >,
    ) -> DomainResult<u64> {
        let conn = self.conn()?;
        let q = attachment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now_utc()));
        let res = f(q)
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .exec(&conn)
            .await?;
        Ok(res.rows_affected)
    }

    async fn mark_failed(&self, scope: &AccessScope, id: Uuid, code: &str) {
        let code = code.to_owned();
        let res = self
            .update_attachment(scope, id, |q| {
                q.col_expr(attachment::Column::Status, Expr::value("failed"))
                    .col_expr(attachment::Column::ErrorCode, Expr::value(Some(code)))
            })
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, attachment_id = %id, "failed to mark attachment failed");
        }
    }

    /// Process an upload after the body was read.
    ///
    /// # Errors
    /// See the upload error table (DESIGN §3.3).
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    pub async fn upload_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        t: UploadTarget,
        filename: &str,
        class: FileClass,
        data: Bytes,
    ) -> DomainResult<attachment::Model> {
        let filename = mime::truncate_filename(if filename.trim().is_empty() {
            "upload"
        } else {
            filename
        });
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        self.check_provider_match(&t).await?;
        let id = Uuid::now_v7();
        let row = self
            .insert_pending(ctx, &t, id, &filename, &class, size)
            .await?;
        let s2s = self.llm.s2s().await.unwrap_or_else(|| ctx.clone());

        // Provider upload.
        let provider_name = format!(
            "{}_{}.{}",
            t.chat.id,
            id,
            mime::extension_for(&filename, &class.content_type)
        );
        let file_id = match self
            .llm
            .upload_file(
                ctx,
                &t.storage,
                &provider_name,
                &class.content_type,
                data.clone(),
            )
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %id, "provider file upload failed");
                self.mark_failed(&t.scope, id, "upload_failed").await;
                return Err(storage_unavailable());
            }
        };
        let fid = file_id.clone();
        self.update_attachment(&t.scope, id, |q| {
            q.col_expr(attachment::Column::Status, Expr::value("uploaded"))
                .col_expr(attachment::Column::ProviderFileId, Expr::value(Some(fid)))
        })
        .await?;

        // Anthropic secondary copy of images.
        if class.is_image
            && let Some(alias) = &t.anthropic_alias
            && data.len() <= self.cfg.thumbnail.max_decode_bytes
        {
            let res = self
                .llm
                .upload_anthropic_file(
                    ctx,
                    alias,
                    &provider_name,
                    &class.content_type,
                    data.clone(),
                )
                .await;
            let (sid, st) = match res {
                Ok(sid) => (Some(sid), "uploaded"),
                Err(e) => {
                    tracing::warn!(error = %e, "anthropic secondary upload failed");
                    (None, "failed")
                }
            };
            self.update_attachment(&t.scope, id, |q| {
                q.col_expr(attachment::Column::SecondaryFileId, Expr::value(sid))
                    .col_expr(attachment::Column::SecondaryStatus, Expr::value(st))
                    .col_expr(
                        attachment::Column::SecondaryProviderKind,
                        Expr::value(Some("anthropic".to_owned())),
                    )
            })
            .await?;
        }

        if class.is_image {
            let cfg = self.cfg.thumbnail.clone();
            let thumb = tokio::task::spawn_blocking(move || thumbnail::generate(&data, &cfg))
                .await
                .ok()
                .flatten();
            self.update_attachment(&t.scope, id, |q| {
                let q = q.col_expr(attachment::Column::Status, Expr::value("ready"));
                match thumb {
                    Some(th) => q
                        .col_expr(
                            attachment::Column::ImgThumbnail,
                            Expr::value(Some(th.bytes)),
                        )
                        .col_expr(
                            attachment::Column::ImgThumbnailWidth,
                            Expr::value(Some(i32::try_from(th.width).unwrap_or(0))),
                        )
                        .col_expr(
                            attachment::Column::ImgThumbnailHeight,
                            Expr::value(Some(i32::try_from(th.height).unwrap_or(0))),
                        ),
                    None => q,
                }
            })
            .await?;
            return self.reload(&t.scope, row.id).await;
        }

        if class.for_file_search {
            let vs = match self.chat_vector_store(ctx, &t).await {
                Ok(vs) => vs,
                Err(e) => {
                    self.mark_failed(&t.scope, id, "vector_store_failed").await;
                    self.spawn_delete_file(s2s, t.storage.clone(), file_id.clone());
                    return Err(e);
                }
            };
            let mut status = match self
                .llm
                .add_vector_store_file(ctx, &t.storage, &vs, &file_id, &id.to_string())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, attachment_id = %id, "adding the file to the vector store failed");
                    VsFileStatus::Failed("add_failed".into())
                }
            };
            let deadline = t.started + UPLOAD_INDEXING_DEADLINE;
            let mut wait = Duration::from_millis(250);
            while status == VsFileStatus::InProgress {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(wait.min(deadline - now)).await;
                wait = (wait * 2).min(Duration::from_secs(2));
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(
                    remaining,
                    self.llm
                        .get_vector_store_file(ctx, &t.storage, &vs, &file_id),
                )
                .await
                {
                    Err(_) => break,
                    Ok(Ok(s)) => status = s,
                    Ok(Err(e)) if e.is_transient() => {
                        tracing::warn!(error = %e, "transient vector store status read error");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "vector store status read failed");
                        status = VsFileStatus::Failed("read_error".into());
                    }
                }
            }
            match status {
                VsFileStatus::Completed => {
                    self.update_attachment(&t.scope, id, |q| {
                        q.col_expr(attachment::Column::Status, Expr::value("ready"))
                    })
                    .await?;
                }
                VsFileStatus::Failed(why) => {
                    tracing::warn!(attachment_id = %id, status = %why, "document indexing failed");
                    self.mark_failed(&t.scope, id, "indexing_failed").await;
                    self.spawn_delete_file(s2s, t.storage.clone(), file_id.clone());
                    return Err(storage_unavailable());
                }
                VsFileStatus::InProgress => {
                    let state = Arc::clone(self);
                    let scope = t.scope.clone();
                    let storage = t.storage.clone();
                    let ctx = ctx.clone();
                    let row_c = row.clone();
                    tokio::spawn(async move {
                        state
                            .background_indexing(ctx, scope, storage, row_c, vs, file_id)
                            .await;
                    });
                }
            }
            return self.reload(&t.scope, row.id).await;
        }

        // Code-interpreter-only document.
        self.update_attachment(&t.scope, id, |q| {
            q.col_expr(attachment::Column::Status, Expr::value("ready"))
        })
        .await?;
        self.reload(&t.scope, row.id).await
    }

    async fn reload(&self, scope: &AccessScope, id: Uuid) -> DomainResult<attachment::Model> {
        let conn = self.conn()?;
        attachment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("attachment row disappeared"))
    }

    fn spawn_delete_file(&self, ctx: SecurityContext, st: ResolvedStorage, file_id: String) {
        let llm = Arc::clone(&self.llm);
        tokio::spawn(async move {
            if let Err(e) = llm.delete_file(&ctx, &st, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    /// Get or create the chat's vector store (creation protocol).
    #[allow(
        clippy::cognitive_complexity,
        reason = "the creation protocol is one sequence of guarded steps"
    )]
    async fn chat_vector_store(
        &self,
        ctx: &SecurityContext,
        t: &UploadTarget,
    ) -> DomainResult<String> {
        let scope = &t.scope;
        for _restart in 0..3 {
            let conn = self.conn()?;
            let row = chat_vector_store::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(t.chat.id)))
                .one(&conn)
                .await?;
            if let Some(r) = &row {
                if r.provider != t.storage.backend_label {
                    return Err(DomainError::already_exists(
                        Res::Attachment,
                        "provider_mismatch",
                        "the chat's documents are stored with another provider backend",
                    ));
                }
                if let Some(vs) = &r.vector_store_id {
                    return Ok(vs.clone());
                }
                if (now_utc() - r.created_at).whole_seconds() > STALE_PLACEHOLDER_SECS {
                    chat_vector_store::Entity::delete_many()
                        .secure()
                        .scope_with(scope)
                        .filter(
                            Condition::all()
                                .add(chat_vector_store::Column::Id.eq(r.id))
                                .add(chat_vector_store::Column::VectorStoreId.is_null()),
                        )
                        .exec(&conn)
                        .await?;
                    continue;
                }
                return self.poll_vector_store(scope, t.chat.id).await;
            }
            let row_id = Uuid::now_v7();
            let am = chat_vector_store::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(t.chat.tenant_id),
                chat_id: Set(t.chat.id),
                vector_store_id: Set(None),
                provider: Set(t.storage.backend_label.clone()),
                file_count: Set(0),
                created_at: Set(now_utc()),
            };
            match secure_insert::<chat_vector_store::Entity>(am, scope, &conn).await {
                Ok(_) => {}
                Err(e) => {
                    let e = DomainError::from(e);
                    if e.is_unique_violation() {
                        return self.poll_vector_store(scope, t.chat.id).await;
                    }
                    return Err(e);
                }
            }
            let created = self
                .llm
                .create_vector_store(ctx, &t.storage, &format!("chat_{}", t.chat.id))
                .await;
            let vs = match created {
                Ok(vs) => vs,
                Err(e) => {
                    tracing::warn!(error = %e, "vector store creation failed");
                    if let Ok(conn) = self.conn()
                        && let Err(e) = chat_vector_store::Entity::delete_many()
                            .secure()
                            .scope_with(scope)
                            .filter(Condition::all().add(chat_vector_store::Column::Id.eq(row_id)))
                            .exec(&conn)
                            .await
                    {
                        tracing::warn!(error = %e, "failed to remove the vector store placeholder");
                    }
                    return Err(storage_unavailable());
                }
            };
            let conn = self.conn()?;
            let res = chat_vector_store::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(
                    chat_vector_store::Column::VectorStoreId,
                    Expr::value(Some(vs.clone())),
                )
                .filter(
                    Condition::all()
                        .add(chat_vector_store::Column::Id.eq(row_id))
                        .add(chat_vector_store::Column::VectorStoreId.is_null()),
                )
                .exec(&conn)
                .await?;
            if res.rows_affected == 1 {
                return Ok(vs);
            }
            let llm = Arc::clone(&self.llm);
            let st = t.storage.clone();
            let c = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = llm.delete_vector_store(&c, &st, &vs).await {
                    tracing::warn!(error = %e, "failed to delete a superseded vector store");
                }
            });
            return self.poll_vector_store(scope, t.chat.id).await;
        }
        Err(storage_unavailable())
    }

    async fn poll_vector_store(&self, scope: &AccessScope, chat_id: Uuid) -> DomainResult<String> {
        let mut wait = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.conn()?;
            let row = chat_vector_store::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
                .one(&conn)
                .await?;
            if let Some(vs) = row.and_then(|r| r.vector_store_id) {
                return Ok(vs);
            }
        }
        Err(storage_unavailable())
    }

    /// Background indexing wait for a document still `in_progress` at the
    /// upload deadline.
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    async fn background_indexing(
        self: Arc<Self>,
        ctx: SecurityContext,
        scope: AccessScope,
        storage: ResolvedStorage,
        row: attachment::Model,
        vs: String,
        file_id: String,
    ) {
        let started = Instant::now();
        let alive_cond = || {
            Condition::all()
                .add(attachment::Column::Id.eq(row.id))
                .add(attachment::Column::Status.eq("uploaded"))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null())
        };
        let mut last_transient: Option<String> = None;
        let outcome: Result<(), String> = 'outer: loop {
            if started.elapsed() >= BG_INDEXING_LIMIT {
                break Err(format!(
                    "indexing timed out{}",
                    last_transient
                        .as_ref()
                        .map(|e| format!(" (last error: {e})"))
                        .unwrap_or_default()
                ));
            }
            // Heartbeat.
            let Ok(conn) = self.conn() else {
                return;
            };
            let hb = attachment::Entity::update_many()
                .secure()
                .scope_with(&scope)
                .col_expr(attachment::Column::UpdatedAt, Expr::value(now_utc()))
                .filter(alive_cond())
                .exec(&conn)
                .await;
            match hb {
                Ok(r) if r.rows_affected == 0 => return,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "indexing heartbeat failed"),
            }
            let round_end = Instant::now() + BG_ROUND;
            let mut wait = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::select! {
                    () = self.cancel.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(Duration::from_secs(5));
                match self
                    .llm
                    .get_vector_store_file(&ctx, &storage, &vs, &file_id)
                    .await
                {
                    Ok(VsFileStatus::Completed) => break 'outer Ok(()),
                    Ok(VsFileStatus::InProgress) => {}
                    Ok(VsFileStatus::Failed(s)) => {
                        break 'outer Err(format!("indexing status {s}"));
                    }
                    Err(e) if e.is_transient() => {
                        if last_transient.is_none() {
                            tracing::warn!(error = %e, "transient indexing status read error");
                        }
                        last_transient = Some(e.to_string());
                    }
                    Err(e) => break 'outer Err(e.to_string()),
                }
                if started.elapsed() >= BG_INDEXING_LIMIT {
                    break;
                }
            }
        };
        match outcome {
            Ok(()) => {
                let mut delay = Duration::from_secs(1);
                for attempt in 0..4 {
                    let res = match self.conn() {
                        Ok(conn) => attachment::Entity::update_many()
                            .secure()
                            .scope_with(&scope)
                            .col_expr(attachment::Column::Status, Expr::value("ready"))
                            .col_expr(attachment::Column::UpdatedAt, Expr::value(now_utc()))
                            .filter(alive_cond())
                            .exec(&conn)
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    match res {
                        Ok(()) => return,
                        Err(e) => {
                            tracing::warn!(error = %e, attempt, "setting attachment ready failed");
                            if attempt < 3 {
                                tokio::time::sleep(delay).await;
                                delay *= 2;
                            }
                        }
                    }
                }
            }
            Err(why) => {
                tracing::warn!(attachment_id = %row.id, reason = %why, "background indexing failed");
                if let Err(e) = self.fail_indexing(&scope, &row, &file_id).await {
                    tracing::error!(error = %e, "failed to record background indexing failure");
                }
            }
        }
    }

    async fn fail_indexing(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
        file_id: &str,
    ) -> DomainResult<()> {
        let outbox = self.outbox.get().await?;
        let scope = scope.clone();
        let row = row.clone();
        let file_id = file_id.to_owned();
        let q = self.cleanup_queue();
        let wake = self
            .write_tx(move |tx| {
                let q = q.clone();
                let scope = scope.clone();
                let row = row.clone();
                let file_id = file_id.clone();
                let outbox = Arc::clone(&outbox);
                Box::pin(async move {
                    let now = now_utc();
                    let res = attachment::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value(Some("indexing_failed".to_owned())),
                        )
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending".to_owned())),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(row.id))
                                .add(attachment::Column::Status.eq("uploaded"))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Ok(None);
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_indexing_failed".into(),
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.attachment_kind.clone(),
                        deleted_at: OffsetDateTime::now_utc(),
                        secondary_ref: None,
                    };
                    Ok(Some(st_enqueue_cleanup(&outbox, tx, &q, &ev).await?))
                })
            })
            .await;
        if let Some(w) = wake? {
            w.fire();
        }
        Ok(())
    }

    /// Attachment cleanup queue and partition count.
    #[must_use]
    pub fn cleanup_queue(&self) -> (String, u32) {
        (
            self.cfg.outbox.cleanup_queue_name.clone(),
            self.cfg.outbox.num_partitions,
        )
    }

    // -----------------------------------------------------------------
    // Get / delete
    // -----------------------------------------------------------------

    #[allow(clippy::similar_names, reason = "conventional names (cond/res/rest)")]
    async fn visible_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
        action: &str,
        include_deleted: bool,
    ) -> DomainResult<(AccessScope, attachment::Model)> {
        let scopes = authz::chat_scopes(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.conn()?;
        repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let mut cond = Condition::all()
            .add(attachment::Column::Id.eq(attachment_id))
            .add(attachment::Column::ChatId.eq(chat_id))
            .add(attachment::Column::UploadedByUserId.eq(ctx.subject_id()));
        if !include_deleted {
            cond = cond.add(attachment::Column::DeletedAt.is_null());
        }
        let a = attachment::Entity::find()
            .secure()
            .scope_with(&scopes.tenant)
            .filter(cond)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Attachment, attachment_id))?;
        Ok((scopes.tenant, a))
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn get_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<attachment::Model> {
        Ok(self
            .visible_attachment(ctx, chat_id, attachment_id, actions::READ_ATTACHMENT, false)
            .await?
            .1)
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    #[allow(clippy::similar_names, reason = "conventional names (cond/res/rest)")]
    pub async fn delete_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<()> {
        let (scope, a) = self
            .visible_attachment(
                ctx,
                chat_id,
                attachment_id,
                actions::DELETE_ATTACHMENT,
                true,
            )
            .await?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let conn = self.conn()?;
        let refs = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
            )
            .count(&conn)
            .await?;
        if refs > 0 {
            return Err(DomainError::already_exists(
                Res::Attachment,
                "attachment_locked",
                "the attachment is referenced by a submitted message",
            ));
        }
        let outbox = self.outbox.get().await?;
        let secondary = match (&a.secondary_file_id, a.secondary_status.as_str()) {
            (Some(fid), "uploaded") => {
                let alias = self
                    .llm
                    .registry
                    .entries
                    .iter()
                    .find(|(_, e)| e.kind == ProviderKind::AnthropicMessages)
                    .map(|(_, e)| self.llm.registry.alias_for(e, Some(a.tenant_id)));
                alias.map(|alias| SecondaryRef {
                    file_id: fid.clone(),
                    provider_kind: "anthropic".into(),
                    upstream_alias: alias,
                })
            }
            _ => None,
        };
        let a = Arc::new(a);
        let q = self.cleanup_queue();
        let wake = self
            .write_tx(move |tx| {
                let q = q.clone();
                let a = Arc::clone(&a);
                let scope = scope.clone();
                let outbox = Arc::clone(&outbox);
                let secondary = secondary.clone();
                Box::pin(async move {
                    let now = now_utc();
                    let res = attachment::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending".to_owned())),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(a.id))
                                .add(attachment::Column::DeletedAt.is_null()),
                        )
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Ok(None);
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_deleted".into(),
                        tenant_id: a.tenant_id,
                        chat_id: a.chat_id,
                        attachment_id: a.id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: OffsetDateTime::now_utc(),
                        secondary_ref: secondary,
                    };
                    Ok(Some(st_enqueue_cleanup(&outbox, tx, &q, &ev).await?))
                })
            })
            .await?;
        if let Some(w) = wake {
            w.fire();
        }
        Ok(())
    }
}

/// Enqueue an attachment cleanup event using the configured queue.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn st_enqueue_cleanup(
    outbox: &toolkit_db::outbox::Outbox,
    tx: &toolkit_db::DbTx<'_>,
    q: &(String, u32),
    ev: &AttachmentCleanupEvent,
) -> DomainResult<toolkit_db::outbox::Wake> {
    super::outbox::enqueue_json(
        outbox,
        tx,
        &q.0,
        super::outbox::partition_for(ev.tenant_id, q.1),
        super::outbox::PT_ATTACHMENT_CLEANUP,
        ev,
    )
    .await
    .map_err(|e| match e {
        EnqueueError::TooLarge(m) | EnqueueError::Other(m) => DomainError::internal(m),
    })
}
