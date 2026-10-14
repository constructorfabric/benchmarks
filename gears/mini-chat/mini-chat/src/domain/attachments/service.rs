//! The attachments service of the upload and get flows
//! (`cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`,
//! `-upload-image` and `-get-attachment`).
//!
//! The upload runs admission, the intake (model and storage provider resolution, the concurrency
//! permit, the multipart headers, MIME type, kind and purposes), the per-chat limits and insert,
//! the streamed provider upload, then the file search path (vector store, indexing wait, background
//! indexing) or the image path (secondary copy, thumbnail). Every return path after the permit was
//! taken runs the upload request completion exactly once.
//!
//! The work after the permit was taken runs on a spawned task of the service (the handler only
//! awaits its result), so a dropped handler future (the gateway request timeout or a client
//! disconnect) cannot abandon an upload between the provider file upload and the row write: the
//! task always runs the failure handling or the completion. Every fallible step after the
//! provider file upload succeeded deletes the provider file best effort when it fails.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{RagConfig, ThumbnailConfig};
use crate::domain::attachments::background::{
    BackgroundDeps, BackgroundTask, run_background_indexing,
};
use crate::domain::attachments::completion::{UploadOutcome, UploadPermits, UploadRequest};
use crate::domain::attachments::failure::UploadFailure;
use crate::domain::attachments::indexing::{IndexedFile, IndexingOutcome, wait_for_indexing};
use crate::domain::attachments::intake::{
    Purposes, effective_limit_bytes, kind_of, normalize_filename, provider_filename, resolve_mime,
    resolve_secondary, resolve_storage, route_purposes,
};
use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::metrics::AttachmentMetrics;
use crate::domain::attachments::model::{AttachmentKind, AttachmentRecord, SecondaryStatus};
use crate::domain::attachments::provider::{
    FileUploadMeta, StorageProvider, StorageTarget, UploadBody, UploadBodyError, UploadError,
};
use crate::domain::attachments::store::{
    AttachmentCleanupQueue, AttachmentStore, DeleteOutcome, DeleteRequest, NewAttachment,
    SecondaryUpdate,
};
use crate::domain::attachments::thumbnail::generate_thumbnail;
use crate::domain::attachments::timing::Timer;
use crate::domain::attachments::upload_body::BodyCounter;
use crate::domain::attachments::vector_store::{
    ChatVectorStoreRepo, VectorStoreDeps, VectorStoreFailure, provision_vector_store,
};
use crate::domain::authz::{Operation, PepService};
use crate::domain::chats::{ChatStore, scope_of};
use crate::domain::error::{AttachmentError, DomainError, ResourceKind};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;

/// The configuration values the upload reads (`rag`, `thumbnail`).
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct AttachmentSettings {
    pub rag: RagConfig,
    pub thumbnail: ThumbnailConfig,
}

/// The request body of an upload, handed over unread: the `Content-Type` header value and the
/// body byte stream.
#[domain_model]
pub struct UploadInput {
    pub content_type: Option<String>,
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

/// The collaborators of [`AttachmentsService`].
#[domain_model]
pub struct AttachmentsDeps {
    pub pep: Arc<PepService>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub chats: Arc<dyn ChatStore>,
    pub store: Arc<dyn AttachmentStore>,
    pub vector_stores: Arc<dyn ChatVectorStoreRepo>,
    pub providers: Arc<ProviderRegistry>,
    pub storage: Arc<dyn StorageProvider>,
    pub cleanup: Arc<dyn AttachmentCleanupQueue>,
    pub permits: Arc<UploadPermits>,
    pub clock: Arc<dyn Clock>,
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn AttachmentMetrics>,
    pub settings: AttachmentSettings,
    /// The gear cancellation token: stops the background indexing tasks.
    pub shutdown: CancellationToken,
}

/// The attachments service.
#[domain_model]
pub struct AttachmentsService {
    deps: AttachmentsDeps,
    /// The spawned background indexing tasks, so a gear stop can wait for them.
    background: TaskTracker,
}

/// What the intake resolved for a request.
struct Resolved {
    snapshot: PolicySnapshot,
    entry: ModelCatalogEntry,
    target: StorageTarget,
    secondary: Option<StorageTarget>,
}

/// The accepted `file` part: its stream and everything the intake derived from its headers.
struct Accepted {
    field: multer::Field<'static>,
    file: FileFacts,
    purposes: Purposes,
}

/// What the intake derived from the `file` part headers.
struct FileFacts {
    filename: String,
    mime: String,
    kind: AttachmentKind,
    /// The effective per-file byte limit.
    limit: u64,
}

/// Lock a mutex whose holder cannot leave the value inconsistent.
fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AttachmentsService {
    /// Create the service over its collaborators.
    #[must_use]
    pub fn new(deps: AttachmentsDeps) -> Self {
        Self {
            deps,
            background: TaskTracker::new(),
        }
    }

    /// Wait until every spawned background indexing task has finished. The tasks stop when the
    /// gear cancellation token fires; the gear stop cancels it first and bounds this wait.
    pub async fn drain_background(&self) {
        self.background.close();
        self.background.wait().await;
    }

    /// `POST /v1/chats/{id}/attachments`: the upload flows (document and image).
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { Chat }`, `InvalidModel`, the intake, limit and provider
    /// Problems of the error contract, or `Internal`.
    pub async fn upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: UploadInput,
    ) -> Result<AttachmentRecord, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-start-instant
        // the monotonic reading of the request start; the indexing deadline is 25 s after it
        let started = d.timer.monotonic();
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-start-instant
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-refusal
        // a refusal, or no chat in the owner scope, is returned before any body byte is read
        let scope = scope_of(
            d.pep
                .authorize(ctx, Operation::UploadAttachment, Some(chat_id))
                .await?,
        )?;
        let chat = d.chats.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-refused
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-admission
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-admission
        let resolved = self.resolve_providers(ctx, &chat.model).await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit-busy
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-busy
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-no-permit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-return-no-permit
        // no permit is free: nothing is held, released or recorded
        let mut request = d
            .permits
            .try_acquire()
            .ok_or(DomainError::Attachment(AttachmentError::UploadBusy))?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-return-no-permit
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-no-permit
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-busy
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit-busy
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit
        // the body-consuming pipeline runs on its own task, bound to the gear shutdown token: a
        // dropped handler future (request timeout, client disconnect) cannot abandon it after the
        // provider upload, so the permit, the metrics and the cleanup always run
        let service = Arc::clone(self);
        let ctx = ctx.clone();
        let input = UploadInput {
            content_type: input.content_type,
            body: self.bound_to_shutdown(input.body),
        };
        let pipeline = self.background.spawn(async move {
            let mut mime = String::new();
            let result = service
                .run_upload(&ctx, chat_id, input, started, &resolved, &mut mime)
                .await;
            Self::complete(&mut request, &mime, &result);
            result
        });
        match pipeline.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(%error, "the upload task did not finish");
                Err(DomainError::internal("the upload task did not finish"))
            }
        }
    }

    /// The request body that ends with an error when the gear stops, so an upload in flight fails
    /// through the normal failure path instead of outliving the gear.
    fn bound_to_shutdown(
        &self,
        body: BoxStream<'static, Result<Bytes, std::io::Error>>,
    ) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
        let stop = self.deps.shutdown.clone();
        let stopped = stop.clone();
        body.take_until(stop.cancelled_owned())
            .chain(
                futures::stream::once(async move { stopped.is_cancelled() }).filter_map(
                    |cancelled| async move {
                        cancelled.then(|| Err(std::io::Error::other("the gear is stopping")))
                    },
                ),
            )
            .boxed()
    }

    /// The upload request completion on the return path of the flow.
    fn complete(
        request: &mut UploadRequest,
        mime: &str,
        result: &Result<AttachmentRecord, DomainError>,
    ) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit-rule
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-permit-rule
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-permit-rule
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-intake
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-intake
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-limit
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-limit
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-complete-uploaded
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-metrics
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-upload
        // every 201 is `ok` (`uploaded` included) and every Problem is `error`
        match result {
            Ok(record) => request.complete(
                mime,
                UploadOutcome::Created {
                    bytes: u64::try_from(record.size_bytes).unwrap_or_default(),
                },
            ),
            Err(_) => request.complete(mime, UploadOutcome::Problem),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-upload
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-metrics
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-metrics
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-metrics
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-complete-uploaded
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-limit
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-limit
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-intake
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-intake
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-permit-rule
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-permit-rule
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-permit-rule
    }

    /// The model entry, the storage provider and the optional secondary target of the chat.
    async fn resolve_providers(
        &self,
        ctx: &SecurityContext,
        model: &str,
    ) -> Result<Resolved, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-model
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-policy-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-policy
        // the resolver error is returned as is: there is no fallback provider and no fallback
        // limit
        let snapshot = d.policy.current_snapshot(ctx.subject_id()).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-policy
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-policy-failed
        // the model is looked up without the enabled filter and without a quota cascade
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == model)
            .cloned();
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-model
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-model-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-invalid-model
        let entry = entry.ok_or_else(|| DomainError::InvalidModel {
            model: model.to_owned(),
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-invalid-model
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-model-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-storage
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-storage-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-storage
        // no storage-capable provider is a generic internal error
        let target = resolve_storage(&d.providers, &entry.provider_id, ctx.subject_tenant_id())?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-storage
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-storage-missing
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-storage
        let secondary =
            resolve_secondary(&d.providers, &entry.provider_id, ctx.subject_tenant_id());
        Ok(Resolved {
            snapshot,
            entry,
            target,
            secondary,
        })
    }
}

/// The identity of the row being uploaded: scoped values that never come from request input.
struct Row<'a> {
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    uploader_id: Uuid,
    target: &'a StorageTarget,
    /// Whether the row still counts toward the pending gauge: the gauge goes down exactly once,
    /// where the row leaves `pending`.
    pending: AtomicBool,
}

impl Row<'_> {
    /// Whether the caller is the one that must decrement the pending gauge (once per row).
    fn release_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

impl AttachmentsService {
    /// Everything after the permit was taken: intake, insert, provider upload and the purpose
    /// paths. `mime_label` receives the resolved MIME type as soon as it is known, for the
    /// `kind` label of the upload metric.
    async fn run_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: UploadInput,
        started: Duration,
        resolved: &Resolved,
        mime_label: &mut String,
    ) -> Result<AttachmentRecord, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-intake
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-intake
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-intake-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-intake-rejected
        // an intake Problem: no row was written and no file byte was read
        let accepted = self.intake(input, resolved, mime_label).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-intake-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-intake-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-intake
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-intake
        let Accepted {
            field,
            file,
            purposes,
        } = accepted;
        let FileFacts {
            filename,
            mime,
            kind,
            ..
        } = &file;
        let kind = *kind;
        let row = Row {
            tenant_id: ctx.subject_tenant_id(),
            chat_id,
            id: Uuid::new_v4(),
            uploader_id: ctx.subject_id(),
            target: &resolved.target,
            pending: AtomicBool::new(false),
        };

        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-insert
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-insert
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-limit-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-limit-rejected
        // a refused limit leaves no row (the 429 is the error of the insert)
        let limits = ChatLimits::from_rag(&d.settings.rag);
        d.store
            .insert_pending(
                &NewAttachment {
                    id: row.id,
                    tenant_id: row.tenant_id,
                    chat_id,
                    uploaded_by_user_id: row.uploader_id,
                    filename: filename.clone(),
                    content_type: mime.clone(),
                    storage_backend: resolved.target.storage_backend.clone(),
                    kind,
                    for_file_search: purposes.for_file_search,
                    for_code_interpreter: purposes.for_code_interpreter,
                    now: d.clock.now(),
                },
                &limits,
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-limit-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-limit-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-insert
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-insert
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        row.pending.store(true, Ordering::SeqCst);
        d.metrics.pending_add(1);
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-commit

        let (provider_file_id, counter) = self.provider_upload(&row, &file, field, &limits).await?;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-no-index
        if kind == AttachmentKind::Image {
            // an image is never added to a vector store: it is model input only on a turn that
            // lists it in `attachment_ids`
            return self
                .finish_image(
                    &row,
                    &provider_file_id,
                    filename,
                    mime,
                    resolved.secondary.as_ref(),
                    counter,
                )
                .await;
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-no-index
        self.finish_document(&row, purposes, &provider_file_id, started)
            .await
    }

    /// The upload intake after the permit: the multipart headers, filename, MIME type, kind and
    /// purposes, and the effective per-file limit. No file byte is read and no row is written.
    async fn intake(
        &self,
        input: UploadInput,
        resolved: &Resolved,
        mime_label: &mut String,
    ) -> Result<Accepted, DomainError> {
        let rag = &self.deps.settings.rag;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-multipart
        let boundary = input
            .content_type
            .as_deref()
            .and_then(|value| multer::parse_boundary(value).ok())
            .ok_or(DomainError::Attachment(AttachmentError::BoundaryRequired))?;
        let mut multipart = multer::Multipart::new(input.body, boundary);
        let field = loop {
            match multipart.next_field().await {
                Err(_) => return Err(DomainError::Attachment(AttachmentError::MultipartError)),
                Ok(None) => return Err(DomainError::Attachment(AttachmentError::MissingFile)),
                Ok(Some(field)) if field.name() == Some("file") => break field,
                Ok(Some(_)) => {}
            }
        };
        let declared = field
            .content_type()
            .map(ToString::to_string)
            .ok_or(DomainError::Attachment(AttachmentError::MissingContentType))?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-multipart
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-filename
        let filename = normalize_filename(field.file_name());
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-filename
        // the MIME type (extension inference, the CSV remap and the allowlist) is resolved in one
        // step; its result is the label of the upload metric from here on
        let mime = resolve_mime(&declared, &filename, rag.allow_csv_upload)?;
        mime_label.clone_from(&mime);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-kind
        let kind = kind_of(&mime);
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-kind
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-images-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-images-disabled
        // no vision capability check runs at upload
        if kind == AttachmentKind::Image && resolved.snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled { subject: "images" });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-images-disabled
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-images-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-purposes
        let purposes = route_purposes(
            &mime,
            kind,
            resolved.snapshot.kill_switches.disable_code_interpreter,
            resolved.entry.general_config.tool_support.code_interpreter,
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-purposes
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-limit
        let limit =
            effective_limit_bytes(kind, rag, resolved.entry.general_config.max_file_size_mb);
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-limit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return
        Ok(Accepted {
            field,
            file: FileFacts {
                filename,
                mime,
                kind,
                limit,
            },
            purposes,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return
    }
}

impl AttachmentsService {
    /// The provider file upload: stream the `file` part to the provider with a byte counter, move
    /// the row to `uploaded`, then run the storage re-check. Returns the provider file id and the
    /// counter (which holds the kept image bytes). Every failure of this step has already been
    /// handled by [`fail`](Self::fail) when it is returned.
    async fn provider_upload(
        &self,
        row: &Row<'_>,
        file: &FileFacts,
        field: multer::Field<'static>,
        limits: &ChatLimits,
    ) -> Result<(String, Arc<Mutex<BodyCounter>>), DomainError> {
        let d = &self.deps;
        let FileFacts {
            filename,
            mime,
            kind,
            limit,
        } = file;
        let (kind, limit) = (*kind, *limit);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-stream
        // the bytes are counted as they stream; an image keeps a copy only while the count stays
        // at or below `thumbnail.max_decode_bytes`
        let keep = (kind == AttachmentKind::Image).then_some(d.settings.thumbnail.max_decode_bytes);
        let counter = Arc::new(Mutex::new(BodyCounter::new(limit, keep)));
        let body: UploadBody = Box::pin(futures::stream::unfold(
            (field, Arc::clone(&counter), false),
            |(mut field, counter, done)| async move {
                if done {
                    return None;
                }
                match field.chunk().await {
                    Ok(Some(bytes)) => {
                        let pushed = locked(&counter).push(&bytes);
                        match pushed {
                            Ok(()) => Some((Ok(bytes), (field, counter, false))),
                            Err(_) => {
                                Some((Err(UploadBodyError::LimitExceeded), (field, counter, true)))
                            }
                        }
                    }
                    Ok(None) => None,
                    Err(_) => Some((Err(UploadBodyError::Read), (field, counter, true))),
                }
            },
        ));
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-stream
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-build
        let meta = FileUploadMeta {
            provider_filename: provider_filename(row.chat_id, row.id, filename),
            content_type: mime.clone(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-build
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-provider-upload
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-provider-upload
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-post
        let uploaded = d.storage.upload_file(row.target, &meta, body).await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-post
        let count = locked(&counter).count();
        let provider_file_id = match uploaded {
            Ok(file_id) => file_id,
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-too-large
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return-too-large
            // the provider request was dropped at once: there is no provider file id
            Err(UploadError::Body(UploadBodyError::LimitExceeded)) => {
                return Err(self
                    .fail(row, None, None, UploadFailure::FileTooLarge)
                    .await);
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return-too-large
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-too-large
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return-failed
            // not retried: a client retry is a new upload with a new attachment and file id
            Err(_) => {
                return Err(self
                    .fail(row, None, None, UploadFailure::UploadFailed)
                    .await);
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return-failed
              // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-failed
        };
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-provider-upload
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-provider-upload
        let size = i64::try_from(count).unwrap_or(i64::MAX);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-set-uploaded
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-uploaded
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        // the provider file id is not durable until this write commits: when it cannot be
        // committed the failure handling still deletes the provider file best effort and fails the
        // row if it can
        let marked = d
            .store
            .mark_uploaded(
                row.tenant_id,
                row.id,
                &provider_file_id,
                size,
                d.clock.now(),
            )
            .await;
        if let Err(error) = marked {
            return Err(self
                .abort(
                    row,
                    Some(&provider_file_id),
                    None,
                    UploadFailure::UploadFailed,
                    error,
                )
                .await);
        }
        // the row left `pending`: this is where the gauge goes down
        if row.release_pending() {
            d.metrics.pending_add(-1);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-uploaded
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-set-uploaded
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-recheck
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-recheck
        let within = match d
            .store
            .storage_recheck(row.tenant_id, row.chat_id, row.id, size, limits)
            .await
        {
            Ok(within) => within,
            Err(error) => {
                return Err(self
                    .abort(
                        row,
                        Some(&provider_file_id),
                        None,
                        UploadFailure::UploadFailed,
                        error,
                    )
                    .await);
            }
        };
        if !within {
            return Err(self
                .fail(
                    row,
                    Some(&provider_file_id),
                    None,
                    UploadFailure::StorageLimitExceeded,
                )
                .await);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-recheck
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-recheck
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-internal-id
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return
        // the provider file id stays inside the service: no API, event, error text or metric
        // label carries it
        Ok((provider_file_id, counter))
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-return
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload:p1:inst-pfu-internal-id
    }

    /// The tail of an image upload: the secondary copy (Anthropic chats), the thumbnail and the
    /// `ready` write.
    async fn finish_image(
        &self,
        row: &Row<'_>,
        provider_file_id: &str,
        filename: &str,
        mime: &str,
        secondary: Option<&StorageTarget>,
        counter: Arc<Mutex<BodyCounter>>,
    ) -> Result<AttachmentRecord, DomainError> {
        let d = &self.deps;
        let kept = Arc::try_unwrap(counter)
            .ok()
            .and_then(|counter| counter.into_inner().ok())
            .and_then(BodyCounter::into_kept);
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-secondary
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-skip
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-return-skip
        // not an Anthropic chat, or the bytes were not kept: `secondary_status` stays
        // `not_attempted`
        let secondary_file: Option<(&StorageTarget, String)> =
            if let (Some(target), Some(bytes)) = (secondary, kept.as_deref()) {
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-return-skip
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-skip
                let (outcome, file_id) = self
                    .secondary_copy(row, target, filename, mime, bytes)
                    .await;
                tracing::debug!(status = outcome.as_str(), "the secondary copy finished");
                file_id.map(|file_id| (target, file_id))
            } else {
                None
            };
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-secondary
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-thumbnail
        // the decode is CPU work: it runs off the async workers; a failed task is a skip
        let config = d.settings.thumbnail.clone();
        let mime_owned = mime.to_owned();
        let thumbnail = match kept {
            Some(bytes) => tokio::task::spawn_blocking(move || {
                generate_thumbnail(Some(&bytes), &mime_owned, &config)
            })
            .await
            .ok()
            .flatten(),
            None => None,
        };
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-thumbnail
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-set-ready
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-ready
        let marked = d
            .store
            .mark_ready(row.tenant_id, row.id, thumbnail.as_ref(), d.clock.now())
            .await;
        if let Err(error) = marked {
            // the row is not `ready`: fail it and delete both provider copies best effort
            return Err(self
                .abort(
                    row,
                    Some(provider_file_id),
                    secondary_file
                        .as_ref()
                        .map(|(target, file_id)| (*target, file_id.as_str())),
                    UploadFailure::UploadFailed,
                    error,
                )
                .await);
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-ready
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-set-ready
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-ready
        d.store
            .get(row.tenant_id, row.chat_id, row.id, row.uploader_id)
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-ready
    }

    /// The secondary copy of an image in the Anthropic Files API. A failure never fails the
    /// upload: the state is recorded and the cause is logged without provider text. Returns the
    /// recorded `secondary_status`.
    async fn secondary_copy(
        &self,
        row: &Row<'_>,
        target: &StorageTarget,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> (SecondaryStatus, Option<String>) {
        let d = &self.deps;
        let record = |update: SecondaryUpdate| async move {
            if let Err(error) = d
                .store
                .set_secondary(row.tenant_id, row.id, &update, d.clock.now())
                .await
            {
                tracing::warn!(%error, "the secondary copy state was not written");
            }
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-pending
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-pending
        record(SecondaryUpdate::Pending).await;
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-pending
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-upload
        let uploaded = d
            .storage
            .upload_secondary_image(target, filename, mime, Bytes::copy_from_slice(bytes))
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-upload
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-return
        match uploaded {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-set-uploaded
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-uploaded
            Ok(file_id) => {
                record(SecondaryUpdate::Uploaded {
                    file_id: file_id.clone(),
                })
                .await;
                (SecondaryStatus::Uploaded, Some(file_id))
            }
            // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-uploaded
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-set-uploaded
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-set-failed
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-failed
            Err(failure) => {
                // the attachment itself still becomes `ready`
                tracing::warn!(?failure, "the secondary copy upload failed");
                record(SecondaryUpdate::Failed).await;
                (SecondaryStatus::Failed, None)
            } // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy:p1:inst-sc-failed
              // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-set-failed
              // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-failed
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy:p1:inst-sec-return
    }
}

impl AttachmentsService {
    /// The tail of a document upload: the file search path (vector store, in-request indexing
    /// wait, background indexing) when the document is routed to `file_search`, then the `ready`
    /// write.
    async fn finish_document(
        &self,
        row: &Row<'_>,
        purposes: Purposes,
        provider_file_id: &str,
        started: Duration,
    ) -> Result<AttachmentRecord, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-file-search-path
        if purposes.for_file_search {
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-file-search-path
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-vector-store
            let deps = VectorStoreDeps {
                repo: &*d.vector_stores,
                provider: &*d.storage,
                clock: &*d.clock,
                timer: &*d.timer,
                target: row.target,
            };
            let provisioned = provision_vector_store(&deps, row.tenant_id, row.chat_id).await;
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-vector-store
            let vector_store_id = match provisioned {
                Ok(store) => store,
                Err(VectorStoreFailure::VectorStoreFailed) => {
                    return Err(self
                        .fail(
                            row,
                            Some(provider_file_id),
                            None,
                            UploadFailure::VectorStoreFailed,
                        )
                        .await);
                }
                Err(VectorStoreFailure::ProviderMismatch) => {
                    return Err(self
                        .fail(
                            row,
                            Some(provider_file_id),
                            None,
                            UploadFailure::ProviderMismatch,
                        )
                        .await);
                }
                // a database failure of the provisioning: the provider file is still deleted
                Err(VectorStoreFailure::Internal(error)) => {
                    return Err(self
                        .abort(
                            row,
                            Some(provider_file_id),
                            None,
                            UploadFailure::VectorStoreFailed,
                            error,
                        )
                        .await);
                }
            };
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-indexing-wait
            let file = IndexedFile {
                vector_store_id: vector_store_id.clone(),
                provider_file_id: provider_file_id.to_owned(),
                attachment_id: row.id,
            };
            let indexed =
                wait_for_indexing(&*d.storage, &*d.timer, row.target, &file, started).await;
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-indexing-wait
            match indexed {
                IndexingOutcome::Ready => {}
                IndexingOutcome::Failed => {
                    return Err(self
                        .fail(
                            row,
                            Some(provider_file_id),
                            None,
                            UploadFailure::IndexingFailed,
                        )
                        .await);
                }
                // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-still-indexing
                IndexingOutcome::StillInProgress => {
                    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-still-indexing
                    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-spawn-background
                    // the row stays `uploaded`; the task holds no upload permit
                    self.spawn_background(row, vector_store_id, provider_file_id);
                    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-spawn-background
                    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-uploaded
                    return d
                        .store
                        .get(row.tenant_id, row.chat_id, row.id, row.uploader_id)
                        .await;
                    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-uploaded
                }
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-ci-path
        // a document routed only to `code_interpreter` is not added to the vector store: its
        // provider file id is offered to the tool at stream time
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-ci-path
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-set-ready
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-ready
        let marked = d
            .store
            .mark_ready(row.tenant_id, row.id, None, d.clock.now())
            .await;
        if let Err(error) = marked {
            // the row is not `ready`: fail it and delete the provider file best effort (the
            // provider drops the file from its vector stores with it)
            return Err(self
                .abort(
                    row,
                    Some(provider_file_id),
                    None,
                    UploadFailure::UploadFailed,
                    error,
                )
                .await);
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-ready
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-set-ready
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-ready
        d.store
            .get(row.tenant_id, row.chat_id, row.id, row.uploader_id)
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-ready
    }

    /// Spawn the background indexing task for the row; it stops with the gear cancellation token.
    fn spawn_background(&self, row: &Row<'_>, vector_store_id: String, provider_file_id: &str) {
        let d = &self.deps;
        let background = BackgroundDeps {
            provider: Arc::clone(&d.storage),
            store: Arc::clone(&d.store),
            cleanup: Arc::clone(&d.cleanup),
            clock: Arc::clone(&d.clock),
            timer: Arc::clone(&d.timer),
            metrics: Arc::clone(&d.metrics),
        };
        let task = BackgroundTask {
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            attachment_id: row.id,
            vector_store_id,
            provider_file_id: provider_file_id.to_owned(),
            target: row.target.clone(),
        };
        let cancel = d.shutdown.clone();
        self.background.spawn(async move {
            run_background_indexing(&background, &task, &cancel).await;
        });
    }

    /// The upload failure handling of a row inserted for this request: fail the row, delete the
    /// provider file (and an uploaded secondary copy) best effort, and return the Problem to
    /// return. The caller runs the upload request completion with the returned error.
    ///
    /// This is the in-request failure handling that the failure rule of both flows routes the
    /// provider upload and file search failures to.
    async fn fail(
        &self,
        row: &Row<'_>,
        provider_file_id: Option<&str>,
        secondary: Option<(&StorageTarget, &str)>,
        failure: UploadFailure,
    ) -> DomainError {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-failure-rule
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-upload-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-mark-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-pending-failed
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-uploaded-failed
        match d
            .store
            .mark_failed(row.tenant_id, row.id, failure.error_code(), d.clock.now())
            .await
        {
            // the gauge goes down once, for a row that did not leave `pending` at
            // `inst-pfu-set-uploaded` (including a `mark_uploaded` that committed but reported an
            // error); a row that already left it is not decremented again
            Ok(Some(_)) => {
                if row.release_pending() {
                    d.metrics.pending_add(-1);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "the failed row was not written"),
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-uploaded-failed
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-pending-failed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-mark-failed
        Self::delete_after_failure(d.storage.as_ref(), row, provider_file_id, secondary).await;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-visible
        // the row stays visible through the get flow with `status` `failed` and its `error_code`
        // and counts toward neither per-chat limit
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-visible
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-failure
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-failure
        failure.into_error()
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-return-failure
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-return-failure
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-upload-failed
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-failed
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-failure-rule
    }

    /// The failure handling of an error (not a provider outcome) that surfaced after the provider
    /// file was uploaded or the row was inserted: the same row and provider cleanup as
    /// [`fail`](Self::fail), but the original error is what the request returns.
    async fn abort(
        &self,
        row: &Row<'_>,
        provider_file_id: Option<&str>,
        secondary: Option<(&StorageTarget, &str)>,
        failure: UploadFailure,
        error: DomainError,
    ) -> DomainError {
        tracing::error!(%error, "the upload failed after the provider file upload");
        // the returned Problem of the failure kind is not the request's answer
        drop(self.fail(row, provider_file_id, secondary, failure).await);
        error
    }

    /// The best-effort deletes after an in-request failure: the provider file and an uploaded
    /// secondary copy. Not retried; the outcomes are only logged.
    async fn delete_after_failure(
        storage: &dyn StorageProvider,
        row: &Row<'_>,
        provider_file_id: Option<&str>,
        secondary: Option<(&StorageTarget, &str)>,
    ) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-has-file
        let Some(file) = provider_file_id else {
            return;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-has-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-delete-file
        // best effort and not retried: the outcome is only logged
        if storage.delete_file(row.target, file).await.is_err() {
            tracing::warn!("the best-effort provider file delete failed");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-delete-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-delete-secondary
        if let Some((target, file_id)) = secondary
            && storage
                .delete_secondary_file(target, file_id)
                .await
                .is_err()
        {
            tracing::warn!("the best-effort secondary copy delete failed");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-delete-secondary
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { Chat }`, `NotFound { Attachment }` or `Internal`.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentRecord, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-admission
        let scope = scope_of(
            d.pep
                .authorize(ctx, Operation::ReadAttachment, Some(chat_id))
                .await?,
        )?;
        d.chats.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-select
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-missing
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-return-404
        // an unknown, soft-deleted, other-chat and other-uploader attachment are the same
        // not-found
        let record = d
            .store
            .get(
                ctx.subject_tenant_id(),
                chat_id,
                attachment_id,
                ctx.subject_id(),
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-missing
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-select
        Ok(record)
    }
}

impl AttachmentsService {
    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`: admission with the action
    /// `delete_attachment` on the chat, then the delete transaction. `Deleted` and
    /// `AlreadyDeleted` are the successes (204); no provider call is made.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { Chat }`, `NotFound { Attachment }`,
    /// `Attachment(Locked)`, `OutboxPayloadTooLarge` or `Internal`.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<DeleteOutcome, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-admission
        let scope = scope_of(
            d.pep
                .authorize(ctx, Operation::DeleteAttachment, Some(chat_id))
                .await?,
        )?;
        let chat = d.chats.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-admission
        // the Anthropic alias of the secondary copy, resolved for the tenant of the chat's model;
        // without it the cleanup message carries no `secondary_ref` and the copy stays behind
        let secondary_upstream_alias = match d.policy.current_snapshot(ctx.subject_id()).await {
            Ok(snapshot) => snapshot
                .model_catalog
                .iter()
                .find(|entry| entry.id == chat.model)
                .and_then(|entry| {
                    resolve_secondary(&d.providers, &entry.provider_id, ctx.subject_tenant_id())
                })
                .map(|target| target.upstream_alias),
            Err(error) => {
                tracing::warn!(%error, "the policy snapshot is unavailable; a secondary copy is not cleaned up");
                None
            }
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-transaction
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-500
        let outcome = d
            .store
            .delete_attachment(
                &DeleteRequest {
                    tenant_id: ctx.subject_tenant_id(),
                    chat_id,
                    attachment_id,
                    uploader_id: ctx.subject_id(),
                    now: d.clock.now(),
                    secondary_upstream_alias,
                },
                Arc::clone(&d.cleanup),
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-500
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-transaction
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-match
        match outcome {
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-404
            DeleteOutcome::NotFound => Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
                id: Some(attachment_id.to_string()),
            }),
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-404
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-locked
            DeleteOutcome::Locked => Err(DomainError::Attachment(AttachmentError::Locked)),
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-locked
            // both successes are 204: an already deleted attachment enqueues nothing
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-idempotent
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-204
            DeleteOutcome::AlreadyDeleted | DeleteOutcome::Deleted => Ok(outcome),
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-204
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-idempotent
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-match
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
