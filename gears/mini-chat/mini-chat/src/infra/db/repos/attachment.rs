//! `attachments` queries: the message-list summary, the chat facts that decide
//! the tools of a turn, and the attachment links of a sent message.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, ExprTrait, FromQueryResult, IntoActiveModel, QueryFilter,
    QueryOrder, QuerySelect,
};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::model::{AttachmentKind, AttachmentStatus, error_codes};
use crate::infra::db::entities::{attachment, chat_vector_store, message_attachment};

/// Attachment facts of a chat that decide the tools of a turn (DESIGN §3.3
/// "Retrieval Model", §4 "Code Interpreter Tool Availability").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatToolFacts {
    /// At least one ready, non-deleted attachment with `for_file_search`.
    pub ready_docs: bool,
    /// Provider file ids of the ready, non-deleted `for_code_interpreter`
    /// attachments (upload order).
    pub xlsx_file_ids: Vec<String>,
    /// The chat's vector store id, when one was created.
    pub vector_store_id: Option<String>,
    /// `provider_file_id -> (attachment_id, filename)` of the ready, non-deleted
    /// attachments (file citation mapping).
    pub citation_map: HashMap<String, (Uuid, String)>,
}

/// Content type of every stored thumbnail (DESIGN §3.7 `img_thumbnail`: WebP).
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// Server-generated preview of a ready image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgThumbnail {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// Lightweight attachment metadata embedded in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    /// Present only for images with `status = ready` that have a thumbnail.
    pub img_thumbnail: Option<ImgThumbnail>,
}

#[derive(Debug, FromQueryResult)]
struct SummaryRow {
    id: Uuid,
    filename: String,
    attachment_kind: String,
    status: String,
    img_thumbnail: Option<Vec<u8>>,
    img_thumbnail_width: Option<i32>,
    img_thumbnail_height: Option<i32>,
}

impl From<SummaryRow> for AttachmentSummary {
    fn from(r: SummaryRow) -> Self {
        let ready_image = r.attachment_kind == AttachmentKind::Image.as_str()
            && r.status == AttachmentStatus::Ready.as_str();
        let img_thumbnail = match (
            ready_image,
            r.img_thumbnail,
            r.img_thumbnail_width,
            r.img_thumbnail_height,
        ) {
            (true, Some(data), Some(width), Some(height)) => Some(ImgThumbnail {
                width,
                height,
                data,
            }),
            _ => None,
        };
        Self {
            attachment_id: r.id,
            kind: r.attachment_kind,
            filename: r.filename,
            status: r.status,
            img_thumbnail,
        }
    }
}

/// Queries over `attachments`.
pub struct AttachmentRepo;

impl AttachmentRepo {
    /// Non-deleted attachments linked to each of `message_ids` (messages without
    /// any are absent), in link order. Two batch queries per call.
    ///
    /// # Errors
    /// Database failures.
    pub async fn summaries_for_messages(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> DomainResult<HashMap<Uuid, Vec<AttachmentSummary>>> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let scope = AccessScope::for_tenant(tenant_id);
        let links = message_attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .order_by_asc(message_attachment::Column::CreatedAt)
            .order_by_asc(message_attachment::Column::AttachmentId)
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        if links.is_empty() {
            return Ok(HashMap::new());
        }

        let attachment_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(attachment_ids))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .project_all(runner, |q| {
                q.select_only()
                    .column(attachment::Column::Id)
                    .column(attachment::Column::Filename)
                    .column(attachment::Column::AttachmentKind)
                    .column(attachment::Column::Status)
                    .column(attachment::Column::ImgThumbnail)
                    .column(attachment::Column::ImgThumbnailWidth)
                    .column(attachment::Column::ImgThumbnailHeight)
                    .into_model::<SummaryRow>()
            })
            .await?;
        let by_id: HashMap<Uuid, AttachmentSummary> = rows
            .into_iter()
            .map(|r| (r.id, AttachmentSummary::from(r)))
            .collect();

        let mut out: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        for link in links {
            if let Some(summary) = by_id.get(&link.attachment_id) {
                out.entry(link.message_id)
                    .or_default()
                    .push(summary.clone());
            }
        }
        Ok(out)
    }
}

impl AttachmentRepo {
    /// Tool facts of `chat_id` (DB only): ready documents, code interpreter files,
    /// the vector store id and the citation map.
    ///
    /// # Errors
    /// Database failures.
    pub async fn chat_tool_facts(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<ChatToolFacts> {
        let scope = AccessScope::for_tenant(tenant_id);
        let ready = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq(AttachmentStatus::Ready.as_str()))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .order_by_asc(attachment::Column::CreatedAt)
            .order_by_asc(attachment::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        let vector_store_id = chat_vector_store::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::ChatId.eq(chat_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_not_null()),
            )
            .secure()
            .scope_with(&scope)
            .one(runner)
            .await?
            .and_then(|vs| vs.vector_store_id);
        let mut facts = ChatToolFacts {
            ready_docs: ready.iter().any(|a| a.for_file_search),
            vector_store_id,
            ..ChatToolFacts::default()
        };
        for a in ready {
            let Some(file_id) = a.provider_file_id else {
                continue;
            };
            if a.for_code_interpreter {
                facts.xlsx_file_ids.push(file_id.clone());
            }
            facts.citation_map.insert(file_id, (a.id, a.filename));
        }
        Ok(facts)
    }

    /// Attachments of `chat_id` among `ids` (deleted ones included, so the
    /// caller can tell why an id is not usable).
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_in_chat(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> DomainResult<Vec<attachment::Model>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(ids.iter().copied())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .all(runner)
            .await?)
    }

    /// Ids of the non-deleted attachments linked to `message_id`, in link order
    /// (the attachments a retry/edit copies to the new user message).
    ///
    /// # Errors
    /// Database failures.
    pub async fn live_linked_ids(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> DomainResult<Vec<Uuid>> {
        let scope = AccessScope::for_tenant(tenant_id);
        let links = message_attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.eq(message_id)),
            )
            .order_by_asc(message_attachment::Column::CreatedAt)
            .order_by_asc(message_attachment::Column::AttachmentId)
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        if links.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let live = Self::find_in_chat(runner, tenant_id, chat_id, &ids).await?;
        Ok(ids
            .into_iter()
            .filter(|id| live.iter().any(|a| a.id == *id && a.deleted_at.is_none()))
            .collect())
    }

    /// Record the `message_attachments` links of a message (in `ids` order).
    ///
    /// # Errors
    /// Scope violations and database failures.
    pub async fn link_to_message(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
        ids: &[Uuid],
        now: DateTime<Utc>,
    ) -> DomainResult<()> {
        let scope = AccessScope::for_tenant(tenant_id);
        for &attachment_id in ids {
            let row = message_attachment::Model {
                tenant_id,
                chat_id,
                message_id,
                attachment_id,
                created_at: now,
            };
            secure_insert::<message_attachment::Entity>(row.into_active_model(), &scope, runner)
                .await?;
        }
        Ok(())
    }
}

/// Status / cleanup guard of a row that is still being indexed: `uploaded`, not
/// handed to cleanup, not deleted.
fn still_uploaded(id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::Id.eq(id))
        .add(attachment::Column::Status.eq(AttachmentStatus::Uploaded.as_str()))
        .add(attachment::Column::CleanupStatus.is_null())
        .add(attachment::Column::DeletedAt.is_null())
}

/// Per-chat usage counted against the RAG limits (non-deleted, non-failed rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatUsage {
    pub documents: u64,
    pub total_bytes: i64,
}

/// Upload / indexing / deletion writes.
impl AttachmentRepo {
    /// The attachment `id` of `chat_id` (deleted rows included).
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_one(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> DomainResult<Option<attachment::Model>> {
        Ok(attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.eq(id)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// Document count and total size of the non-deleted, non-failed attachments of `chat_id`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn chat_usage(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<ChatUsage> {
        #[derive(Debug, FromQueryResult)]
        struct Row {
            attachment_kind: String,
            size_bytes: Option<i64>,
        }
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Status.ne(AttachmentStatus::Failed.as_str())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .project_all(runner, |q| {
                q.select_only()
                    .column(attachment::Column::AttachmentKind)
                    .column(attachment::Column::SizeBytes)
                    .into_model::<Row>()
            })
            .await?;
        Ok(rows.iter().fold(ChatUsage::default(), |mut u, r| {
            if r.attachment_kind == AttachmentKind::Document.as_str() {
                u.documents += 1;
            }
            u.total_bytes = u.total_bytes.saturating_add(r.size_bytes.unwrap_or(0));
            u
        }))
    }

    /// Insert a new attachment row.
    ///
    /// # Errors
    /// Scope violations and database failures.
    pub async fn insert(runner: &impl DBRunner, row: attachment::Model) -> DomainResult<()> {
        let scope = AccessScope::for_tenant(row.tenant_id);
        secure_insert::<attachment::Entity>(row.into_active_model(), &scope, runner).await?;
        Ok(())
    }

    /// `pending` → `uploaded` with the provider file id, unless the row was
    /// deleted or handed to cleanup (chat deletion) meanwhile.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_uploaded(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::Status,
                Expr::value(AttachmentStatus::Uploaded.as_str()),
            )
            .col_expr(
                attachment::Column::ProviderFileId,
                Expr::value(provider_file_id),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.eq(AttachmentStatus::Pending.as_str()))
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// `uploaded` → `ready` (with the image thumbnail, if any), unless the row
    /// was deleted or handed to cleanup meanwhile.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_ready(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        thumbnail: Option<ImgThumbnail>,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let mut update = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::Status,
                Expr::value(AttachmentStatus::Ready.as_str()),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
        if let Some(t) = thumbnail {
            update = update
                .col_expr(attachment::Column::ImgThumbnail, Expr::value(t.data))
                .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(t.width))
                .col_expr(
                    attachment::Column::ImgThumbnailHeight,
                    Expr::value(t.height),
                );
        }
        let res = update
            .filter(still_uploaded(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// `pending` / `uploaded` → `failed` with `error_code`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_failed(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        error_code: &str,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::Status,
                Expr::value(AttachmentStatus::Failed.as_str()),
            )
            .col_expr(attachment::Column::ErrorCode, Expr::value(error_code))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(attachment::Column::Id.eq(id)).add(
                attachment::Column::Status.is_in([
                    AttachmentStatus::Pending.as_str(),
                    AttachmentStatus::Uploaded.as_str(),
                ]),
            ))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Record the secondary (Anthropic) copy state: `secondary_status`,
    /// `secondary_file_id` and `secondary_provider_kind`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn set_secondary(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        status: &str,
        file_id: Option<&str>,
        provider_kind: Option<&str>,
        now: DateTime<Utc>,
    ) -> DomainResult<()> {
        attachment::Entity::update_many()
            .col_expr(attachment::Column::SecondaryStatus, Expr::value(status))
            .col_expr(attachment::Column::SecondaryFileId, Expr::value(file_id))
            .col_expr(
                attachment::Column::SecondaryProviderKind,
                Expr::value(provider_kind),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Refresh `updated_at` of a row that is still being indexed (background
    /// indexing heartbeat); `false` when the row is no longer `uploaded`, was
    /// deleted or was handed to cleanup.
    ///
    /// # Errors
    /// Database failures.
    pub async fn touch_uploaded(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(still_uploaded(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Background indexing failure: `uploaded` → `failed` (`indexing_failed`)
    /// with `cleanup_status = 'pending'`, unless the row was deleted or handed
    /// to cleanup meanwhile.
    ///
    /// # Errors
    /// Database failures.
    pub async fn fail_indexing_for_cleanup(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::Status,
                Expr::value(AttachmentStatus::Failed.as_str()),
            )
            .col_expr(
                attachment::Column::ErrorCode,
                Expr::value(error_codes::INDEXING_FAILED),
            )
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(still_uploaded(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Whether any message references the attachment (`message_attachments`).
    ///
    /// # Errors
    /// Database failures.
    pub async fn is_referenced(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> DomainResult<bool> {
        Ok(message_attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::AttachmentId.eq(id)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?
            .is_some())
    }

    /// Soft-delete the attachment and hand it to cleanup
    /// (`deleted_at = updated_at = cleanup_updated_at = now`, `cleanup_status = 'pending'`);
    /// `false` when it was already deleted.
    ///
    /// # Errors
    /// Database failures.
    pub async fn soft_delete_for_cleanup(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(attachment::Column::DeletedAt, Expr::value(now))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}

/// Upload reaper queries (DESIGN B.9.5).
impl AttachmentRepo {
    /// Up to `limit` abandoned-upload candidates across tenants: `pending` or
    /// `uploaded`, not deleted, not owned by cleanup, `updated_at < cutoff`,
    /// oldest `updated_at` first. Advisory only: [`Self::fail_abandoned`] re-checks
    /// every predicate.
    ///
    /// # Errors
    /// Database failures.
    pub async fn stale_uploads(
        runner: &impl DBRunner,
        cutoff: DateTime<Utc>,
        limit: u64,
    ) -> DomainResult<Vec<attachment::Model>> {
        Ok(attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Status.is_in(UPLOADING))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by_asc(attachment::Column::UpdatedAt)
            .order_by_asc(attachment::Column::Id)
            .limit(limit)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(runner)
            .await?)
    }

    /// Reaper CAS: `from_status` → `failed` (`upload_abandoned`, `updated_at = now`),
    /// only while the row is still in `from_status`, not deleted, not owned by
    /// cleanup and `updated_at < cutoff`. With `hand_to_cleanup` the same update
    /// sets `cleanup_status = 'pending'` (the row has a provider file to delete).
    /// `false` when the row changed meanwhile.
    ///
    /// # Errors
    /// Database failures.
    pub async fn fail_abandoned(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        from_status: &str,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
        hand_to_cleanup: bool,
    ) -> DomainResult<bool> {
        let mut update = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::Status,
                Expr::value(AttachmentStatus::Failed.as_str()),
            )
            .col_expr(
                attachment::Column::ErrorCode,
                Expr::value(error_codes::UPLOAD_ABANDONED),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
        if hand_to_cleanup {
            update = update
                .col_expr(
                    attachment::Column::CleanupStatus,
                    Expr::value(cleanup_status::PENDING),
                )
                .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now));
        }
        let res = update
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.eq(from_status))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}

/// Statuses of an upload that has not finished.
const UPLOADING: [&str; 2] = [
    AttachmentStatus::Pending.as_str(),
    AttachmentStatus::Uploaded.as_str(),
];

/// `cleanup_status` values (DESIGN §3.6 "Attachment cleanup state machine").
pub mod cleanup_status {
    pub const PENDING: &str = "pending";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// Outcome of recording a failed provider delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupFailure {
    /// `cleanup_attempts` after the increment.
    pub attempts: i32,
    /// The attempt budget is exhausted: `cleanup_status = 'failed'`.
    pub failed: bool,
}

/// Cleanup state writes (outbox cleanup handlers).
impl AttachmentRepo {
    /// The attachments of `chat_id` (deleted ones included) whose
    /// `cleanup_status` is `status`, in upload order.
    ///
    /// # Errors
    /// Database failures.
    pub async fn with_cleanup_status(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        status: &str,
    ) -> DomainResult<Vec<attachment::Model>> {
        Ok(attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::CleanupStatus.eq(status)),
            )
            .order_by_asc(attachment::Column::CreatedAt)
            .order_by_asc(attachment::Column::Id)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .all(runner)
            .await?)
    }

    /// Provider cleanup finished: `cleanup_status = 'done'`, `cleanup_updated_at = now`.
    /// `false` when the row is gone or already `done`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_cleanup_done(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(cleanup_status::DONE),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(not_done(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Record a failed provider delete: `cleanup_attempts + 1`, `last_cleanup_error`,
    /// `cleanup_updated_at = now`; the status becomes `failed` when the new count
    /// reaches `max_attempts`, else it is left unchanged. `None` when the row is
    /// gone or already `done`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn record_cleanup_failure(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        error: &str,
        max_attempts: i32,
        now: DateTime<Utc>,
    ) -> DomainResult<Option<CleanupFailure>> {
        let status = Expr::case(
            Expr::col(attachment::Column::CleanupAttempts)
                .add(1)
                .gte(max_attempts),
            cleanup_status::FAILED,
        )
        .finally(Expr::col(attachment::Column::CleanupStatus));
        let rows = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupAttempts,
                Expr::col(attachment::Column::CleanupAttempts).add(1),
            )
            .col_expr(attachment::Column::LastCleanupError, Expr::value(error))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .col_expr(attachment::Column::CleanupStatus, status.into())
            .filter(not_done(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec_with_returning(runner)
            .await?;
        Ok(rows.into_iter().next().map(|a| CleanupFailure {
            attempts: a.cleanup_attempts,
            failed: a.cleanup_status.as_deref() == Some(cleanup_status::FAILED),
        }))
    }
}

/// The row `id`, unless its cleanup is already `done`.
fn not_done(id: Uuid) -> Condition {
    Condition::all().add(attachment::Column::Id.eq(id)).add(
        Condition::any()
            .add(attachment::Column::CleanupStatus.is_null())
            .add(attachment::Column::CleanupStatus.ne(cleanup_status::DONE)),
    )
}
