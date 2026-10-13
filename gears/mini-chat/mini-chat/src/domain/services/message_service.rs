//! Message list (DESIGN section 3.3, List Messages).

use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::enums::{AttachmentKind, AttachmentStatus, MessageRole, ReactionKind};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction};
use crate::infra::db::entities::{attachment, message};
use crate::infra::db::repos::{attachment_repo, chat_repo, message_repo, reaction_repo};

/// MIME type of stored thumbnails (DESIGN section 3.7, `img_thumbnail`).
const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// Image thumbnail (`image/webp`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThumbnailView {
    pub content_type: &'static str,
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// Attachment summary embedded in a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    pub attachment_id: Uuid,
    pub kind: AttachmentKind,
    pub filename: String,
    pub status: AttachmentStatus,
    pub img_thumbnail: Option<ThumbnailView>,
}

/// A message as the API shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRole,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<ReactionKind>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub created_at: OffsetDateTime,
}

pub struct MessageService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<dyn AuthzPort>,
}

impl MessageService {
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, authz: Arc<dyn AuthzPort>) -> Self {
        Self { db, authz }
    }

    /// Page of the chat's live messages (default `created_at asc`, `id asc`),
    /// each with its live attachments and the caller's reaction.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat), `Query` (bad `OData` query),
    /// `Internal` for a stored message without `request_id` or an unknown
    /// stored enum value, database failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::ListMessages, Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let page = message_repo::list_page(&conn, chat.tenant_id, chat.id, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut attachments =
            attachment_repo::live_for_messages(&conn, chat.tenant_id, chat.id, &ids).await?;
        let reactions =
            reaction_repo::mine_for_messages(&conn, chat.tenant_id, ctx.subject_id(), &ids).await?;

        let mut items = Vec::with_capacity(page.items.len());
        for row in page.items {
            let linked = attachments.remove(&row.id).unwrap_or_default();
            let my_reaction = reactions.get(&row.id).copied();
            items.push(message_view(row, linked, my_reaction)?);
        }
        Ok(Page::new(items, page.page_info))
    }
}

fn message_view(
    row: message::Model,
    linked: Vec<attachment::Model>,
    my_reaction: Option<ReactionKind>,
) -> Result<MessageView, DomainError> {
    let request_id = row
        .request_id
        .ok_or_else(|| DomainError::Internal(format!("message {} has no request_id", row.id)))?;
    let role = MessageRole::parse(&row.role).ok_or_else(|| {
        DomainError::Internal(format!("message {} has role {}", row.id, row.role))
    })?;
    let attachments = linked
        .into_iter()
        .map(attachment_summary)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MessageView {
        id: row.id,
        request_id,
        role,
        content: row.content,
        attachments,
        // Only assistant messages carry reactions.
        my_reaction: my_reaction.filter(|_| role == MessageRole::Assistant),
        model: row.model,
        input_tokens: (row.input_tokens != 0).then_some(row.input_tokens),
        output_tokens: (row.output_tokens != 0).then_some(row.output_tokens),
        created_at: row.created_at,
    })
}

fn attachment_summary(a: attachment::Model) -> Result<AttachmentSummaryView, DomainError> {
    let kind = AttachmentKind::parse(&a.attachment_kind).ok_or_else(|| {
        DomainError::Internal(format!(
            "attachment {} has kind {}",
            a.id, a.attachment_kind
        ))
    })?;
    let status = AttachmentStatus::parse(&a.status).ok_or_else(|| {
        DomainError::Internal(format!("attachment {} has status {}", a.id, a.status))
    })?;
    let img_thumbnail = match (
        kind,
        status,
        a.img_thumbnail,
        a.img_thumbnail_width,
        a.img_thumbnail_height,
    ) {
        (AttachmentKind::Image, AttachmentStatus::Ready, Some(data), Some(width), Some(height)) => {
            Some(ThumbnailView {
                content_type: THUMBNAIL_CONTENT_TYPE,
                width,
                height,
                data,
            })
        }
        _ => None,
    };
    Ok(AttachmentSummaryView {
        attachment_id: a.id,
        kind,
        filename: a.filename,
        status,
        img_thumbnail,
    })
}

#[cfg(test)]
#[path = "message_service_tests.rs"]
mod message_service_tests;
