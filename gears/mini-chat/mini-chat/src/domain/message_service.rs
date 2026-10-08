//! Message list (DESIGN "List Messages"): the live messages of a chat with their attachment
//! summaries and the caller's reactions.

use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Authz, ChatAction};
use crate::domain::error::DomainError;
use crate::domain::reaction_service::Reaction;
use crate::infra::db::entity::messages;
use crate::infra::db::repo;
use crate::infra::db::repo::attachments::SummaryRow;
use crate::infra::db::{AttachmentKind, AttachmentStatus, MessageRole};

/// Preview of a ready image attachment (WebP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// Attachment metadata shown on the message that references it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: AttachmentKind,
    pub filename: String,
    pub status: AttachmentStatus,
    /// Only for an image that is `ready` and has a stored preview.
    pub thumbnail: Option<Thumbnail>,
}

/// A message of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRole,
    pub content: String,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<Reaction>,
    pub created_at: OffsetDateTime,
    /// The model that produced an assistant message.
    pub model: Option<String>,
    /// Provider-reported counts; `None` when stored as 0.
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

fn internal(what: &str, value: &str) -> DomainError {
    DomainError::Internal(format!("stored {what} `{value}` is not valid"))
}

impl AttachmentSummary {
    fn from_row(row: SummaryRow) -> Result<Self, DomainError> {
        let kind = AttachmentKind::parse(&row.attachment_kind)
            .ok_or_else(|| internal("attachment kind", &row.attachment_kind))?;
        let status = AttachmentStatus::parse(&row.status)
            .ok_or_else(|| internal("attachment status", &row.status))?;
        let thumbnail = match (
            kind == AttachmentKind::Image && status == AttachmentStatus::Ready,
            row.img_thumbnail,
            row.img_thumbnail_width,
            row.img_thumbnail_height,
        ) {
            (true, Some(data), Some(width), Some(height)) => Some(Thumbnail {
                width,
                height,
                data,
            }),
            _ => None,
        };
        Ok(Self {
            attachment_id: row.id,
            kind,
            filename: row.filename,
            status,
            thumbnail,
        })
    }
}

impl MessageView {
    fn new(
        message: messages::Model,
        attachments: Vec<AttachmentSummary>,
        my_reaction: Option<Reaction>,
    ) -> Result<Self, DomainError> {
        let request_id = message.request_id.ok_or_else(|| {
            DomainError::Internal(format!("message {} has no request_id", message.id))
        })?;
        let role = MessageRole::parse(&message.role)
            .ok_or_else(|| internal("message role", &message.role))?;
        Ok(Self {
            id: message.id,
            request_id,
            role,
            content: message.content,
            attachments,
            my_reaction,
            created_at: message.created_at,
            model: message.model,
            input_tokens: Some(message.input_tokens).filter(|n| *n != 0),
            output_tokens: Some(message.output_tokens).filter(|n| *n != 0),
        })
    }
}

/// Reads the messages of a chat.
pub struct MessageService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
}

impl MessageService {
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, authz: Arc<Authz>) -> Self {
        Self { db, authz }
    }

    /// One page of the live messages of one of the caller's chats, chronological unless the
    /// query orders them. Attachments and the caller's reactions are fetched in batches for the
    /// page.
    ///
    /// # Errors
    /// `ChatNotFound` (also for a deleted or foreign chat), `AccessDenied` / `AuthzUnavailable`,
    /// `OData` for an invalid filter, order or cursor, `Internal` for a message without a
    /// `request_id` or a database failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::ListMessages, Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        repo::chats::load_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound {
                id: chat_id.to_string(),
            })?;

        let tenant_scope = scope.tenant_only();
        let page = repo::messages::list(&conn, &tenant_scope, chat_id, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut attachments =
            repo::attachments::summaries_for_messages(&conn, &tenant_scope, chat_id, &ids).await?;
        let reactions = repo::reactions::for_messages(&conn, &scope, &ids).await?;

        let Page { items, page_info } = page;
        let items = items
            .into_iter()
            .map(|message| {
                let summaries = attachments
                    .remove(&message.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(AttachmentSummary::from_row)
                    .collect::<Result<Vec<_>, _>>()?;
                let reaction = reactions
                    .get(&message.id)
                    .map(|r| Reaction::parse(r).ok_or_else(|| internal("reaction", r)))
                    .transpose()?;
                MessageView::new(message, summaries, reaction)
            })
            .collect::<Result<Vec<_>, DomainError>>()?;
        Ok(Page { items, page_info })
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::infra::db::ts::db_now;

    fn row(kind: &str, status: &str, thumbnail: bool) -> SummaryRow {
        SummaryRow {
            id: Uuid::new_v4(),
            attachment_kind: kind.to_owned(),
            filename: "f".to_owned(),
            status: status.to_owned(),
            img_thumbnail: thumbnail.then(|| vec![1]),
            img_thumbnail_width: thumbnail.then_some(4),
            img_thumbnail_height: thumbnail.then_some(3),
        }
    }

    #[test]
    fn thumbnail_only_for_ready_images_that_have_one() {
        let shown = |kind, status, thumbnail| {
            AttachmentSummary::from_row(row(kind, status, thumbnail))
                .unwrap()
                .thumbnail
                .is_some()
        };
        assert!(shown("image", "ready", true));
        assert!(!shown("image", "ready", false), "no stored preview");
        assert!(!shown("image", "pending", true));
        assert!(!shown("image", "failed", true));
        assert!(!shown("document", "ready", true));
        assert!(AttachmentSummary::from_row(row("sound", "ready", false)).is_err());
        assert!(AttachmentSummary::from_row(row("image", "lost", false)).is_err());
    }

    fn message(request_id: Option<Uuid>, input: i64, output: i64) -> messages::Model {
        messages::Model {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            request_id,
            role: "assistant".to_owned(),
            content: "c".to_owned(),
            content_type: "text".to_owned(),
            token_estimate: 0,
            provider_response_id: None,
            request_kind: "chat".to_owned(),
            features_used: serde_json::json!([]),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: None,
            is_compressed: false,
            created_at: db_now(),
            deleted_at: None,
        }
    }

    #[test]
    fn zero_token_counts_are_absent_and_a_missing_request_id_is_internal() {
        let view = MessageView::new(message(Some(Uuid::new_v4()), 0, 7), Vec::new(), None).unwrap();
        assert_eq!((view.input_tokens, view.output_tokens), (None, Some(7)));

        let err = MessageView::new(message(None, 1, 1), Vec::new(), None).unwrap_err();
        assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    }
}
