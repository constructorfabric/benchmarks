//! Attachment service: upload (synchronous with bounded indexing wait), get,
//! delete, background indexing, thumbnail generation.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{AttachmentDetailDto, AttachmentKindDto};
use crate::domain::chat_service::{attachment_status, policy_err, thumbnail_dto};
use crate::domain::services::ServiceContext;
use crate::error::{DomainError, ResourceKind};
use crate::infra::llm::resolver::ProviderRegistry;
use crate::infra::llm::storage::{StorageClient, VsFileStatus};
use crate::infra::metrics::label;
use crate::infra::outbox::{AttachmentCleanupPayload, OutboxEnqueuer, SecondaryRef};
use crate::infra::storage::entity::attachments;
use crate::infra::storage::repo::attachments::NewAttachment;

/// The attachment service.
pub struct AttachmentService {
    ctx: Arc<ServiceContext>,
    registry: Arc<ProviderRegistry>,
    storage: Arc<StorageClient>,
    upload_semaphore: Arc<tokio::sync::Semaphore>,
    bg_cancel: CancellationToken,
}

/// Supported document/image MIME types.
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
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
    "text/x-java-source",
    "text/javascript",
    "application/typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
];

/// Map a filename extension to a MIME type (for `application/octet-stream`
/// parts).
pub(crate) fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" | "log" | "csv" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java-source",
        "js" => "text/javascript",
        "ts" => "application/typescript",
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

/// A prepared upload (authorization, model, purposes, size limit).
pub struct UploadPlan {
    /// The PDP scope.
    pub scope: toolkit_db::secure::AccessScope,
    /// The chat.
    pub chat: crate::infra::storage::entity::chats::Model,
    /// The chat's model.
    pub model: mini_chat_sdk::ModelCatalogEntry,
    /// Attachment kind.
    pub kind: AttachmentKindDto,
    /// File-search purpose.
    pub for_file_search: bool,
    /// Code-interpreter purpose.
    pub for_code_interpreter: bool,
    /// Effective per-file size limit in bytes.
    pub max_bytes: i64,
    /// Tenant.
    pub tenant_id: Uuid,
    /// User.
    pub user_id: Uuid,
}

/// Whether the MIME type is supported (CSV remapped when allowed).
#[must_use]
pub fn is_supported_mime(content_type: &str, allow_csv: bool) -> bool {
    if content_type == "text/csv" {
        return allow_csv;
    }
    IMAGE_TYPES.contains(&content_type) || DOCUMENT_TYPES.contains(&content_type)
}

/// Attachment kind from a validated MIME type.
#[must_use]
pub fn attachment_kind_of(content_type: &str) -> AttachmentKindDto {
    if IMAGE_TYPES.contains(&content_type) {
        AttachmentKindDto::Image
    } else {
        AttachmentKindDto::Document
    }
}

/// Purpose flags from a validated MIME type (before kill-switch/capability
/// filtering).
#[must_use]
pub fn purposes_of(content_type: &str) -> (bool, bool) {
    if content_type == XLSX {
        (false, true)
    } else if IMAGE_TYPES.contains(&content_type) {
        (false, false)
    } else {
        (true, false)
    }
}

/// A validated upload input.
pub struct UploadInput {
    /// Filename.
    pub filename: String,
    /// MIME type (validated and remapped).
    pub content_type: String,
    /// File bytes.
    pub bytes: bytes::Bytes,
}

impl AttachmentService {
    /// Create the service.
    #[must_use]
    pub fn new(
        ctx: Arc<ServiceContext>,
        registry: Arc<ProviderRegistry>,
        storage: Arc<StorageClient>,
    ) -> Self {
        let permits = usize::from(ctx.config.rag.max_concurrent_uploads);
        Self {
            ctx,
            registry,
            storage,
            upload_semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
            bg_cancel: CancellationToken::new(),
        }
    }

    /// Stop background indexing tasks.
    pub fn stop(&self) {
        self.bg_cancel.cancel();
    }

    /// Prepare an upload: authorization, chat and model resolution, MIME
    /// validation, purpose filtering and the effective size limit. Runs
    /// before the request body is read.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        content_type: &str,
    ) -> Result<UploadPlan, DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "upload_attachment", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        let chat = self
            .ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;

        let version = self.ctx.policy.current_policy_version(user_id).await.map_err(policy_err)?;
        let snapshot = self
            .ctx
            .policy
            .policy_snapshot(user_id, version)
            .await
            .map_err(policy_err)?;
        let model = snapshot
            .model(&chat.model)
            .ok_or(DomainError::InvalidModel)?
            .clone();
        let kill = snapshot.kill_switches;

        if !is_supported_mime(content_type, self.ctx.config.rag.allow_csv_upload) {
            return Err(DomainError::UnsupportedContentType);
        }
        let kind = attachment_kind_of(content_type);
        let (for_file_search, mut for_code_interpreter) = purposes_of(content_type);
        if kind == AttachmentKindDto::Image && kill.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        if for_code_interpreter
            && (kill.disable_code_interpreter || !model.general_config.tool_support.code_interpreter)
        {
            for_code_interpreter = false;
            if !for_file_search {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
        }

        let kind_limit_kb = match kind {
            AttachmentKindDto::Document => self.ctx.config.rag.uploaded_file_max_size_kb,
            AttachmentKindDto::Image => self.ctx.config.rag.uploaded_image_max_size_kb,
        };
        let model_limit_bytes = i64::from(model.general_config.max_file_size_mb) * 1024 * 1024;
        let max_bytes = (i64::from(kind_limit_kb) * 1024).min(model_limit_bytes);

        Ok(UploadPlan {
            scope,
            chat,
            model,
            kind,
            for_file_search,
            for_code_interpreter,
            max_bytes,
            tenant_id,
            user_id,
        })
    }

#[allow(clippy::cognitive_complexity, reason = "the upload pipeline follows the normative state machine")]
    /// Upload an attachment (synchronous, 25 s indexing deadline for
    /// documents).
    #[allow(clippy::too_many_lines, clippy::cast_possible_wrap, clippy::cast_precision_loss)]
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        plan: UploadPlan,
        input: UploadInput,
    ) -> Result<attachments::Model, DomainError> {
        let scope = plan.scope.clone();
        let tenant_id = plan.tenant_id;
        let user_id = plan.user_id;
        let chat_id = plan.chat.id;
        let kind = plan.kind;
        let for_file_search = plan.for_file_search;
        let for_code_interpreter = plan.for_code_interpreter;
        let model = plan.model.clone();

        // Concurrency bound.
        let permit = self
            .upload_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrency)?;

        // Per-chat limits.
        let conn = self.ctx.db.conn()?;
        if kind == AttachmentKindDto::Document {
            let docs = self
                .ctx
                .repos
                .attachments
                .count_documents(&conn, &scope, tenant_id, chat_id)
                .await?;
            if docs >= u64::from(self.ctx.config.rag.max_documents_per_chat) {
                return Err(DomainError::DocumentLimit);
            }
        }
        let total = self
            .ctx
            .repos
            .attachments
            .total_bytes(&conn, &scope, tenant_id, chat_id)
            .await?;
        let limit_bytes = i64::from(self.ctx.config.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        if total + input.bytes.len() as i64 > limit_bytes {
            return Err(DomainError::StorageLimit);
        }

        let upload_start = std::time::Instant::now();
        let kind_label = match kind {
            AttachmentKindDto::Document => "document",
            AttachmentKindDto::Image => "image",
        };

        // Insert the row (pending).
        let attachment_id = Uuid::new_v4();
        let storage_target = self
            .registry
            .resolve_storage(&model.provider_id, &tenant_id.to_string())
            .ok_or_else(|| {
                tracing::error!(
                    "mini-chat: no storage provider for model {} (provider {})",
                    model.id,
                    model.provider_id
                );
                DomainError::Internal
            })?;
        let anthropic_alias = self
            .registry
            .resolve(&model.provider_id, &tenant_id.to_string())
            .filter(|p| p.kind == crate::config::ProviderKind::AnthropicMessages)
            .map(|p| p.alias);
        let row = self
            .ctx
            .repos
            .attachments
            .insert(
                &conn,
                &scope,
                NewAttachment {
                    id: attachment_id,
                    tenant_id,
                    chat_id,
                    uploaded_by_user_id: user_id,
                    filename: input.filename.clone(),
                    content_type: input.content_type.clone(),
                    size_bytes: input.bytes.len() as i64,
                    storage_backend: storage_target.storage_backend.clone(),
                    attachment_kind: kind_label.to_owned(),
                    for_file_search,
                    for_code_interpreter,
                },
            )
            .await?;
        self.metrics_pending_inc();
        let result = self
            .upload_inner(
                ctx,
                &plan.chat.clone(),
                &row,
                &storage_target,
                anthropic_alias.as_deref(),
                &input,
                kind,
                for_file_search,
                upload_start,
            )
            .await;
        drop(permit);
        self.metrics_pending_dec();
        self.ctx.metrics.attachment_upload.add(
            1,
            &[
                label("kind", kind_label),
                label("result", if result.is_ok() { "ok" } else { "error" }),
            ],
        );
        self.ctx
            .metrics
            .attachment_upload_bytes
            .record(input.bytes.len() as f64, &[label("kind", kind_label)]);
        result
    }

    /// The post-insert upload pipeline.
    #[allow(clippy::too_many_arguments)]
    async fn upload_inner(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        row: &attachments::Model,
        storage_target: &crate::infra::llm::resolver::ResolvedStorage,
        anthropic_alias: Option<&str>,
        input: &UploadInput,
        kind: AttachmentKindDto,
        for_file_search: bool,
        upload_start: std::time::Instant,
    ) -> Result<attachments::Model, DomainError> {
        let conn = self.ctx.db.conn()?;
        // Provider upload.
        let provider_file_id = match self
            .storage
            .upload_file(
                ctx,
                storage_target,
                &input.filename,
                &input.content_type,
                input.bytes.clone(),
            )
            .await
        {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!("mini-chat: provider file upload failed: {err}");
                self.ctx
                    .repos
                    .attachments
                    .mark_failed(&conn, row.id, "upload_failed")
                    .await?;
                return Err(DomainError::StorageUnavailable);
            }
        };
        self.ctx
            .repos
            .attachments
            .mark_uploaded(&conn, row.id, &provider_file_id)
            .await?;

        // Anthropic secondary copy for images (chats on Anthropic models).
        if kind == AttachmentKindDto::Image
            && input.bytes.len() <= self.ctx.config.thumbnail.max_decode_bytes
            && let Some(alias) = anthropic_alias
        {
            match self
                .storage
                .anthropic_upload(ctx, alias, &input.filename, &input.content_type, input.bytes.clone())
                .await
            {
                Ok(secondary_id) => {
                    self.ctx
                        .repos
                        .attachments
                        .set_secondary(&conn, row.id, Some(&secondary_id), "uploaded")
                        .await?;
                }
                Err(err) => {
                    tracing::warn!("mini-chat: secondary anthropic upload failed: {err}");
                    self.ctx
                        .repos
                        .attachments
                        .set_secondary(&conn, row.id, None, "failed")
                        .await?;
                }
            }
        }

        if kind == AttachmentKindDto::Image {
            let thumbnail = self.generate_thumbnail(&input.bytes).await;
            self.ctx
                .repos
                .attachments
                .mark_ready(&conn, row.id, thumbnail, false)
                .await?;
            return self
                .ctx
                .repos
                .attachments
                .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), row.tenant_id, row.chat_id, row.id)
                .await?
                .ok_or(DomainError::Internal);
        }

        if !for_file_search {
            // code_interpreter-only document: no vector store indexing.
            self.ctx
                .repos
                .attachments
                .mark_ready(&conn, row.id, None, false)
                .await?;
            return self
                .ctx
                .repos
                .attachments
                .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), row.tenant_id, row.chat_id, row.id)
                .await?
                .ok_or(DomainError::Internal);
        }

        // Document with file_search purpose: vector store indexing.
        let vector_store_id = match self
            .get_or_create_vector_store(ctx, chat, storage_target)
            .await
        {
            Ok(id) => id,
            Err(err) => {
                self.ctx
                    .repos
                    .attachments
                    .mark_failed(&conn, row.id, "vector_store_failed")
                    .await?;
                self.delete_provider_file_quiet(ctx, storage_target, &provider_file_id).await;
                return Err(err);
            }
        };
        let status = match self
            .storage
            .add_file_to_vector_store(ctx, storage_target, &vector_store_id, &provider_file_id, row.id)
            .await
        {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!("mini-chat: add to vector store failed: {err}");
                self.ctx
                    .repos
                    .attachments
                    .mark_failed(&conn, row.id, "indexing_failed")
                    .await?;
                self.delete_provider_file_quiet(ctx, storage_target, &provider_file_id).await;
                return Err(DomainError::StorageUnavailable);
            }
        };
        let deadline = upload_start + std::time::Duration::from_secs(25);
        let mut status = status;
        let mut wait = std::time::Duration::from_millis(250);
        while status == VsFileStatus::InProgress && std::time::Instant::now() < deadline {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(std::time::Duration::from_secs(2));
            match self
                .storage
                .get_vector_store_file(ctx, storage_target, &vector_store_id, &provider_file_id)
                .await
            {
                Ok(s) => status = s,
                Err(err) => {
                    // A transient read error keeps polling.
                    tracing::debug!("mini-chat: indexing status read error: {err}");
                }
            }
        }
        match status {
            VsFileStatus::Completed => {
                self.ctx
                    .repos
                    .attachments
                    .mark_ready(&conn, row.id, None, false)
                    .await?;
                self.ctx
                    .repos
                    .attachments
                    .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), row.tenant_id, row.chat_id, row.id)
                    .await?
                    .ok_or(DomainError::Internal)
            }
            VsFileStatus::Failed => {
                self.ctx
                    .repos
                    .attachments
                    .mark_failed(&conn, row.id, "indexing_failed")
                    .await?;
                self.delete_provider_file_quiet(ctx, storage_target, &provider_file_id).await;
                Err(DomainError::StorageUnavailable)
            }
            VsFileStatus::InProgress => {
                // Indexing continues in the background; the row stays
                // `uploaded` and the client polls.
                self.spawn_background_indexing(
                    ctx.clone(),
                    row.id,
                    row.tenant_id,
                    row.chat_id,
                    storage_target.clone(),
                    vector_store_id,
                    provider_file_id,
                );
                self.ctx
                    .repos
                    .attachments
                    .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), row.tenant_id, row.chat_id, row.id)
                    .await?
                    .ok_or(DomainError::Internal)
            }
        }
    }

    /// Get or create the chat vector store (creation protocol with
    /// placeholder CAS).
    async fn get_or_create_vector_store(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        storage_target: &crate::infra::llm::resolver::ResolvedStorage,
    ) -> Result<String, DomainError> {
        let conn = self.ctx.db.conn()?;
        // Fast path.
        if let Some(row) = self
            .ctx
            .repos
            .vector_stores
            .get(&conn, chat.tenant_id, chat.id)
            .await?
        {
            if row.provider != storage_target.storage_backend {
                return Err(DomainError::ProviderMismatch);
            }
            if let Some(id) = row.vector_store_id {
                return Ok(id);
            }
            // A placeholder exists: stale (>120 s) placeholders are
            // reclaimed; otherwise poll.
            let stale = row.created_at + time::Duration::seconds(120) < time::OffsetDateTime::now_utc();
            if stale {
                self.ctx.repos.vector_stores.delete_by_id(&conn, row.id).await?;
            } else {
                return self.poll_placeholder(ctx, chat, storage_target).await;
            }
        }
        // Winner path: insert the placeholder, create the store, CAS-set.
        let inserted = self
            .ctx
            .repos
            .vector_stores
            .insert_placeholder(&conn, chat.tenant_id, chat.id, &storage_target.storage_backend)
            .await?;
        let Some(placeholder) = inserted else {
            // Loser: poll for the winner's store.
            return self.poll_placeholder(ctx, chat, storage_target).await;
        };
        let store_name = format!("mini-chat-{}", chat.id);
        match self
            .storage
            .create_vector_store(ctx, storage_target, &store_name)
            .await
        {
            Ok(vs_id) => {
                let won = self
                    .ctx
                    .repos
                    .vector_stores
                    .cas_set_id(&conn, placeholder.id, &vs_id)
                    .await?;
                if won {
                    Ok(vs_id)
                } else {
                    // The placeholder was reclaimed while creating; use the
                    // store the chat has now.
                    self.delete_vector_store_quiet(ctx, storage_target, &vs_id).await;
                    self.poll_placeholder(ctx, chat, storage_target).await
                }
            }
            Err(err) => {
                tracing::warn!("mini-chat: vector store creation failed: {err}");
                self.ctx
                    .repos
                    .vector_stores
                    .delete_by_id(&conn, placeholder.id)
                    .await?;
                Err(DomainError::StorageUnavailable)
            }
        }
    }

    /// Loser path: poll the placeholder row until the store id appears.
    async fn poll_placeholder(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        storage_target: &crate::infra::llm::resolver::ResolvedStorage,
    ) -> Result<String, DomainError> {
        let _ = (ctx, storage_target);
        let conn = self.ctx.db.conn()?;
        let mut wait = std::time::Duration::from_millis(50);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            if let Some(row) = self
                .ctx
                .repos
                .vector_stores
                .get(&conn, chat.tenant_id, chat.id)
                .await?
                && let Some(id) = row.vector_store_id
            {
                if row.provider != storage_target.storage_backend {
                    return Err(DomainError::ProviderMismatch);
                }
                return Ok(id);
            }
        }
        Err(DomainError::StorageUnavailable)
    }

    /// Spawn the background indexing wait (up to 10 minutes).
    fn spawn_background_indexing(
        &self,
        ctx: SecurityContext,
        attachment_id: Uuid,
        tenant_id: Uuid,
        chat_id: Uuid,
        storage_target: crate::infra::llm::resolver::ResolvedStorage,
        vector_store_id: String,
        provider_file_id: String,
    ) {
        let svc = self.ctx.clone();
        let storage = Arc::clone(&self.storage);
        let cancel = self.bg_cancel.clone();
        let metrics = self.ctx.metrics.clone();
        tokio::spawn(async move {
            background_indexing(
                svc, storage, cancel, metrics, ctx, attachment_id, tenant_id, chat_id,
                storage_target, vector_store_id, provider_file_id,
            )
            .await;
        });
    }

    /// The secondary (Anthropic) upload alias for a chat, when its model is
    /// served by an `anthropic_messages` provider.
    async fn secondary_alias_for_chat(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
    ) -> Option<String> {
        let conn = self.ctx.db.conn().ok()?;
        let chat = self
            .ctx
            .repos
            .chats
            .get(
                &conn,
                &toolkit_db::secure::AccessScope::allow_all(),
                tenant_id,
                user_id,
                chat_id,
            )
            .await
            .ok()??;
        let version = self.ctx.policy.current_policy_version(user_id).await.ok()?;
        let snapshot = self.ctx.policy.policy_snapshot(user_id, version).await.ok()?;
        let model = snapshot.model(&chat.model)?;
        let provider = self.registry.resolve(&model.provider_id, &tenant_id.to_string())?;
        if provider.kind == crate::config::ProviderKind::AnthropicMessages {
            Some(provider.alias)
        } else {
            None
        }
    }

    /// Delete a provider file, best effort (not retried).
    async fn delete_provider_file_quiet(
        &self,
        ctx: &SecurityContext,
        storage_target: &crate::infra::llm::resolver::ResolvedStorage,
        provider_file_id: &str,
    ) {
        if let Err(err) = self
            .storage
            .delete_file(ctx, storage_target, provider_file_id)
            .await
        {
            tracing::warn!("mini-chat: best-effort provider file delete failed: {err}");
        }
    }

    /// Delete a provider vector store, best effort.
    async fn delete_vector_store_quiet(
        &self,
        ctx: &SecurityContext,
        storage_target: &crate::infra::llm::resolver::ResolvedStorage,
        vector_store_id: &str,
    ) {
        if let Err(err) = self
            .storage
            .delete_vector_store(ctx, storage_target, vector_store_id)
            .await
        {
            tracing::warn!("mini-chat: best-effort vector store delete failed: {err}");
        }
    }

    /// Get an attachment (metadata + status).
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetailDto, DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "read_attachment", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        self.ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let row = self
            .ctx
            .repos
            .attachments
            .get(&conn, &scope, tenant_id, chat_id, attachment_id)
            .await?
            .filter(|r| r.uploaded_by_user_id == user_id)
            .ok_or(DomainError::NotFound(ResourceKind::Attachment))?;
        Ok(attachment_detail(&row))
    }

    /// Delete an attachment (204; idempotent).
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "delete_attachment", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        self.ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let row = self
            .ctx
            .repos
            .attachments
            .get_any(&conn, &scope, tenant_id, chat_id, attachment_id)
            .await?
            // The uploader check runs before the idempotency check.
            .filter(|r| r.uploaded_by_user_id == user_id)
            .ok_or(DomainError::NotFound(ResourceKind::Attachment))?;
        if row.deleted_at.is_some() {
            return Ok(()); // idempotent
        }
        if self
            .ctx
            .repos
            .message_attachments
            .is_referenced(&conn, tenant_id, chat_id, attachment_id)
            .await?
        {
            return Err(DomainError::AttachmentLocked);
        }
        let secondary_ref = match (&row.secondary_file_id, &row.secondary_provider_kind) {
            (Some(file_id), Some(kind)) => self
                .secondary_alias_for_chat(row.tenant_id, row.uploaded_by_user_id, row.chat_id)
                .await
                .map(|upstream_alias| SecondaryRef {
                    file_id: file_id.clone(),
                    provider_kind: kind.clone(),
                    upstream_alias,
                }),
            _ => None,
        };
        let payload = AttachmentCleanupPayload {
            event_type: "attachment_deleted".to_owned(),
            tenant_id,
            chat_id,
            attachment_id,
            provider_file_id: row.provider_file_id.clone(),
            vector_store_id: None,
            storage_backend: row.storage_backend.clone(),
            attachment_kind: row.attachment_kind.clone(),
            deleted_at: crate::api::rest::dto::fmt_ts(&time::OffsetDateTime::now_utc()),
            secondary_ref,
        };
        let svc_ctx = Arc::clone(&self.ctx);
        let result = self
            .ctx
            .db
            .transaction(|tx| {
                let scope = scope.clone();
                let svc_ctx = Arc::clone(&svc_ctx);
                Box::pin(async move {
                    let deleted = svc_ctx
                        .repos
                        .attachments
                        .soft_delete(tx, &scope, tenant_id, chat_id, attachment_id)
                        .await?;
                    if !deleted {
                        return Ok(None);
                    }
                    let wake = svc_ctx.outbox.enqueue_attachment_cleanup(tx, &payload).await?;
                    Ok(Some(wake))
                })
            })
            .await?;
        if let Some(wake) = result {
            wake.fire();
        }
        Ok(())
    }

    fn metrics_pending_inc(&self) {
        self.ctx.metrics.attachments_pending.add(1, &[]);
    }

    fn metrics_pending_dec(&self) {
        self.ctx.metrics.attachments_pending.add(-1, &[]);
    }

    /// Generate a WebP thumbnail (best-effort, bounded by the thumbnail
    /// configuration).
    async fn generate_thumbnail(&self, bytes: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
        let cfg = self.ctx.config.thumbnail.clone();
        if bytes.len() > cfg.max_decode_bytes {
            return None;
        }
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || thumbnail::generate(&bytes, &cfg))
            .await
            .ok()
            .flatten()
    }
}

/// Attachment row -> detail DTO.
#[must_use]
#[must_use]
#[must_use]
#[must_use]
pub fn attachment_detail(row: &attachments::Model) -> AttachmentDetailDto {
    AttachmentDetailDto {
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        status: attachment_status(row),
        kind: attachment_kind_of(&row.content_type),
        error_code: row.error_code.clone(),
        doc_summary: None,
        img_thumbnail: thumbnail_dto(row),
        summary_updated_at: None,
        created_at: row.created_at,
    }
}

#[allow(clippy::redundant_continue, reason = "the polling loop is clearest with explicit continue")]
#[allow(clippy::cognitive_complexity, reason = "the background wait follows the normative state machine")]
/// The background indexing task (rounds of 20 s, up to 10 minutes).
#[allow(clippy::too_many_arguments)]
async fn background_indexing(
    ctx: Arc<ServiceContext>,
    storage: Arc<StorageClient>,
    cancel: CancellationToken,
    metrics: crate::infra::metrics::Metrics,
    sec: SecurityContext,
    attachment_id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    storage_target: crate::infra::llm::resolver::ResolvedStorage,
    vector_store_id: String,
    provider_file_id: String,
) {
    let started = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(600);
    let mut wait = std::time::Duration::from_millis(250);
    let mut last_heartbeat = std::time::Instant::now();
    let mut last_read_error: Option<String> = None;
    loop {
        if cancel.is_cancelled() || started.elapsed() >= timeout {
            break;
        }
        // Heartbeat so the upload reaper leaves the row alone.
        if last_heartbeat.elapsed() >= std::time::Duration::from_secs(20) {
            last_heartbeat = std::time::Instant::now();
            let Ok(conn) = ctx.db.conn() else { break };
            match ctx.repos.attachments.heartbeat(&conn, attachment_id).await {
                Ok(true) => {}
                // The row is gone, no longer uploaded, or owned by chat
                // cleanup: stop without changes.
                _ => return,
            }
        }
        let sleep = wait.min(timeout.saturating_sub(started.elapsed()));
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(sleep) => {}
        }
        wait = (wait * 2).min(std::time::Duration::from_secs(5));
        let status = storage
            .get_vector_store_file(&sec, &storage_target, &vector_store_id, &provider_file_id)
            .await;
        let status = match status {
            Ok(status) => status,
            Err(err) => {
                if last_read_error.is_none() {
                    tracing::warn!("mini-chat: background indexing status read error: {err}");
                }
                last_read_error = Some(err.to_string());
                continue;
            }
        };
        match status {
            VsFileStatus::InProgress => continue,
            VsFileStatus::Completed => {
                // Up to 4 attempts, 1/2/4 s apart.
                let mut ok = false;
                for attempt in 0..4 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1))).await;
                    }
                    let Ok(conn) = ctx.db.conn() else { break };
                    match ctx
                        .repos
                        .attachments
                        .mark_ready(&conn, attachment_id, None, true)
                        .await
                    {
                        Ok(true) => {
                            ok = true;
                            break;
                        }
                        Ok(false) => return, // row deleted or claimed by cleanup
                        Err(_) => continue,
                    }
                }
                metrics.attachment_background_indexing.add(
                    1,
                    &[label("result", if ok { "ready" } else { "set_ready_failed" })],
                );
                return;
            }
            VsFileStatus::Failed => break,
        }
    }
    if cancel.is_cancelled() {
        return;
    }
    // Failure or timeout: fail the row and hand the provider file to the
    // outbox attachment cleanup, in one transaction.
    if started.elapsed() >= std::time::Duration::from_secs(600)
        && let Some(err) = &last_read_error
    {
        tracing::warn!("mini-chat: background indexing timed out; last read error: {err}");
    }
    let repos = ctx.repos.clone();
    let result = ctx
        .db
        .transaction(|tx| {
            let repos = repos.clone();
            let outbox = Arc::clone(&ctx.outbox);
            Box::pin(async move {
                let failed = repos.attachments.fail_indexing(tx, attachment_id).await?;
                if !failed {
                    return Ok(None);
                }
                let payload = AttachmentCleanupPayload {
                    event_type: "attachment_indexing_failed".to_owned(),
                    tenant_id,
                    chat_id,
                    attachment_id,
                    provider_file_id: Some(provider_file_id),
                    vector_store_id: None,
                    storage_backend: storage_target.storage_backend.clone(),
                    attachment_kind: "document".to_owned(),
                    deleted_at: crate::api::rest::dto::fmt_ts(&time::OffsetDateTime::now_utc()),
                    secondary_ref: None,
                };
                let wake = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
                Ok(Some(wake))
            })
        })
        .await;
    match result {
        Ok(Some(wake)) => {
            wake.fire();
            metrics
                .attachment_background_indexing
                .add(1, &[label("result", "failed")]);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::error!("mini-chat: background indexing failure commit failed: {err}");
        }
    }
}

/// Thumbnail generation (blocking; run via `block_in_place`).
pub mod thumbnail {
    use crate::config::ThumbnailConfig;

    /// Generate a WebP thumbnail fitting `width`x`height`, or `None` when
    /// the source is too large or generation fails.
    #[must_use]
    pub fn generate(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<(Vec<u8>, i32, i32)> {
        use image::ImageReader;
        use std::io::Cursor;
        let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
        let (w, h) = reader.into_dimensions().ok()?;
        if u64::from(w) * u64::from(h) > cfg.max_pixels {
            return None;
        }
        if u64::from(w) * u64::from(h) * 4 > cfg.max_decode_bytes as u64 {
            return None;
        }
        let img = image::load_from_memory(bytes).ok()?;
        let thumb = img.thumbnail(cfg.width, cfg.height);
        let mut out = Cursor::new(Vec::new());
        thumb
            .write_to(&mut out, image::ImageFormat::WebP)
            .ok()?;
        let data = out.into_inner();
        if data.len() > cfg.max_bytes {
            return None;
        }
        let (tw, th) = (thumb.width(), thumb.height());
        Some((data, i32::try_from(tw).unwrap_or(0), i32::try_from(th).unwrap_or(0)))
    }
}
