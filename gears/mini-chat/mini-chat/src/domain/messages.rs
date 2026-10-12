//! Messages service (DESIGN 3.3 "List Messages").
//!
//! The chat is looked up with the owner-narrowed scope first (404 for an
//! unknown, foreign or deleted chat). The child tables (`messages`,
//! `attachments`, `message_attachments`, `message_reactions`) are then queried
//! with the tenant scope derived from it plus the `chat_id`; the owner
//! narrowing of the chat scope would blank them because those tables carry no
//! chat owner column.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::{chat_not_found, with_default_order};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{ChatRepository, MessageRepository, ReactionRepository};
use crate::domain::provider::TokenBreakdown;
use crate::domain::reactions::ReactionKind;

/// The inclusive frontier of a chat's thread summary: the last message the
/// summary covers, as `(created_at, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryFrontier {
    /// `created_at` of the last covered message.
    pub created_at: OffsetDateTime,
    /// Id of the last covered message.
    pub message_id: Uuid,
}

/// Author role of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// Written by the user.
    User,
    /// Produced by the model.
    Assistant,
    /// Background / system message.
    System,
}

impl MessageRole {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    /// Parses the stored role.
    ///
    /// # Errors
    /// `Internal` for a value outside the schema contract.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "system" => Ok(Self::System),
            other => Err(DomainError::internal(format!(
                "unknown message role `{other}`"
            ))),
        }
    }
}

/// A live message as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// Message id.
    pub id: Uuid,
    /// Turn correlation key (nullable in the schema).
    pub request_id: Option<Uuid>,
    /// Stored role (`user`, `assistant`, `system`).
    pub role: String,
    /// Message text.
    pub content: String,
    /// Model that produced an assistant message.
    pub model: Option<String>,
    /// Provider-reported input tokens.
    pub input_tokens: i64,
    /// Provider-reported output tokens.
    pub output_tokens: i64,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A message to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    /// Message id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Turn correlation key.
    pub request_id: Uuid,
    /// `user` or `assistant`.
    pub role: MessageRole,
    /// Verbatim content.
    pub content: String,
    /// Effective model (assistant messages).
    pub model: Option<String>,
    /// Provider response id (assistant messages, internal only).
    pub provider_response_id: Option<String>,
    /// Provider reported input tokens.
    pub input_tokens: i64,
    /// Provider reported output tokens.
    pub output_tokens: i64,
    /// Provider reported cache / reasoning breakdown (assistant messages).
    pub breakdown: TokenBreakdown,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A ready, non-deleted attachment of a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored kind (`document`, `image`).
    pub kind: String,
    /// Provider file id (internal only).
    pub provider_file_id: Option<String>,
}

/// An attachment linked to a message, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryRow {
    /// Message the attachment is linked to.
    pub message_id: Uuid,
    /// Attachment id.
    pub attachment_id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored kind (`document`, `image`).
    pub kind: String,
    /// Stored status (`pending`, `uploaded`, `ready`, `failed`).
    pub status: String,
    /// WebP thumbnail bytes, when stored.
    pub thumbnail: Option<Vec<u8>>,
    /// Thumbnail width in pixels.
    pub thumbnail_width: Option<i32>,
    /// Thumbnail height in pixels.
    pub thumbnail_height: Option<i32>,
}

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// A document.
    Document,
    /// An image.
    Image,
}

/// Attachment lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatus {
    /// Upload started.
    Pending,
    /// Provider upload done, indexing running.
    Uploaded,
    /// Usable.
    Ready,
    /// Failed.
    Failed,
}

/// Preview thumbnail of a ready image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    /// WebP bytes.
    pub bytes: Vec<u8>,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// Lightweight attachment metadata embedded in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// Kind.
    pub kind: AttachmentKind,
    /// File name.
    pub filename: String,
    /// Status.
    pub status: AttachmentStatus,
    /// Thumbnail; only for a ready image with stored bytes.
    pub thumbnail: Option<ThumbnailView>,
}

impl AttachmentKind {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }

    /// Parses the stored kind.
    ///
    /// # Errors
    /// `Internal` for a value outside the schema contract.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "document" => Ok(Self::Document),
            "image" => Ok(Self::Image),
            other => Err(DomainError::internal(format!(
                "unknown attachment kind `{other}`"
            ))),
        }
    }
}

impl AttachmentStatus {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// Parses the stored status.
    ///
    /// # Errors
    /// `Internal` for a value outside the schema contract.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "uploaded" => Ok(Self::Uploaded),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            other => Err(DomainError::internal(format!(
                "unknown attachment status `{other}`"
            ))),
        }
    }
}

impl AttachmentSummaryView {
    fn from_row(row: AttachmentSummaryRow) -> Result<Self, DomainError> {
        let kind = AttachmentKind::parse(&row.kind)?;
        let status = AttachmentStatus::parse(&row.status)?;
        let thumbnail = match (
            kind,
            status,
            row.thumbnail,
            row.thumbnail_width,
            row.thumbnail_height,
        ) {
            (
                AttachmentKind::Image,
                AttachmentStatus::Ready,
                Some(bytes),
                Some(width),
                Some(height),
            ) => Some(ThumbnailView {
                bytes,
                width,
                height,
            }),
            _ => None,
        };
        Ok(Self {
            attachment_id: row.attachment_id,
            kind,
            filename: row.filename,
            status,
            thumbnail,
        })
    }
}

/// A message of the list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Message id.
    pub id: Uuid,
    /// Turn correlation key, always present.
    pub request_id: Uuid,
    /// Role.
    pub role: MessageRole,
    /// Text.
    pub content: String,
    /// Model of an assistant message.
    pub model: Option<String>,
    /// Input tokens; `0` is reported as absent by the API.
    pub input_tokens: i64,
    /// Output tokens; `0` is reported as absent by the API.
    pub output_tokens: i64,
    /// Linked, non-deleted attachments.
    pub attachments: Vec<AttachmentSummaryView>,
    /// The caller's reaction (assistant messages only).
    pub my_reaction: Option<ReactionKind>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// Fields of the message list that `$filter` and `$orderby` accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageFilterField {
    /// `created_at` (instant).
    CreatedAt,
    /// `id` (UUID).
    Id,
    /// `role` (string).
    Role,
}

impl FilterField for MessageFilterField {
    const FIELDS: &'static [Self] = &[Self::CreatedAt, Self::Id, Self::Role];

    fn name(&self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Id => "id",
            Self::Role => "role",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::CreatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

/// Lists the messages of a chat.
pub struct MessagesService {
    db: Arc<DBProvider<DomainError>>,
    enforcer: PolicyEnforcer,
}

impl MessagesService {
    /// Creates the service.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, enforcer: PolicyEnforcer) -> Self {
        Self { db, enforcer }
    }

    /// A page of the messages of the caller's chat; the default order is
    /// `created_at asc` with `id` as the tiebreaker.
    ///
    /// # Errors
    /// `NotFound` for an unknown, foreign or deleted chat, `OData` for a bad
    /// query, `Internal` for a stored message without `request_id`, PDP errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::LIST_MESSAGES, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        conn.find_live_chat(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;

        // The chat is the caller's: from here on the tenant scope is enough.
        let tenant_scope = scope.tenant_only();
        let query = with_default_order(query, MessageFilterField::CreatedAt.name(), SortDir::Asc);
        let page = conn
            .list_live_messages(&tenant_scope, chat_id, &query)
            .await?;

        let ids: Vec<Uuid> = page.items.iter().map(|row| row.id).collect();
        let mut attachments = conn
            .attachment_summaries(&tenant_scope, chat_id, &ids)
            .await?;
        let reaction_scope = tenant_scope.ensure_owner(ctx.subject_id());
        let mut reactions = conn
            .reactions_of_user(&reaction_scope, ctx.subject_id(), &ids)
            .await?;

        let mut items = Vec::with_capacity(page.items.len());
        for row in page.items {
            let request_id = row.request_id.ok_or_else(|| {
                DomainError::internal(format!("message {} has no request_id", row.id))
            })?;
            let role = MessageRole::parse(&row.role)?;
            let attachments = attachments
                .remove(&row.id)
                .unwrap_or_default()
                .into_iter()
                .map(AttachmentSummaryView::from_row)
                .collect::<Result<Vec<_>, _>>()?;
            let my_reaction = if role == MessageRole::Assistant {
                reactions
                    .remove(&row.id)
                    .map(|raw| ReactionKind::parse_stored(&raw))
                    .transpose()?
            } else {
                None
            };
            items.push(MessageView {
                id: row.id,
                request_id,
                role,
                content: row.content,
                model: row.model,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                attachments,
                my_reaction,
                created_at: row.created_at,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}

/// Not-found error of a message (unknown, other chat or soft-deleted).
pub(crate) fn message_not_found(id: Uuid) -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Message,
        id: id.to_string(),
    }
}
