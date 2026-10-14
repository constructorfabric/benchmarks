//! The messages list and the turn status read paths (feature
//! `cpt-cf-mini-chat-feature-message-streaming`, slice S1-setup-path): the message read models,
//! the message projection (`cpt-cf-mini-chat-algo-message-streaming-message-projection`), the
//! persistence port of the two reads and the service behind
//! `GET /v1/chats/{id}/messages` and `GET /v1/chats/{id}/turns/{request_id}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{Operation, PepService};
use crate::domain::chats::scope_of;
use crate::domain::error::DomainError;
use crate::domain::turns::{TurnRow, TurnStatusView, project_turn_status};

/// The role of a message as the API reports it.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl MessageRole {
    /// The wire and stored value: `user`, `assistant` or `system`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    /// The role of a stored value, `None` for an unknown one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::User, Self::Assistant, Self::System]
            .into_iter()
            .find(|role| role.as_str() == value)
    }
}

/// The caller's reaction to a message.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    Like,
    Dislike,
}

impl Reaction {
    /// The wire and stored value: `like` or `dislike`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }
}

/// A `messages` row as the list, the replay and the context assembly read it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub id: Uuid,
    /// NULL is legal in the schema; a message with a NULL `request_id` is never served.
    pub request_id: Option<Uuid>,
    /// The stored role text.
    pub role: String,
    pub content: String,
    /// The effective model of an assistant message.
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
}

/// The stored thumbnail of an attachment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailRow {
    pub width: i32,
    pub height: i32,
    /// The stored WebP bytes.
    pub data: Vec<u8>,
}

/// One row of the attachment batch of a message page: `message_attachments` inner-joined to the
/// non-deleted `attachments`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    pub message_id: Uuid,
    pub attachment_id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// The stored thumbnail, when the row has one (whatever the kind and status).
    pub thumbnail: Option<ThumbnailRow>,
}

/// The thumbnail of an attachment summary.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    /// `image/webp`.
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// `AttachmentSummary`: the lightweight attachment metadata embedded in a message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    /// Only for an image with status `ready` and a stored thumbnail.
    pub img_thumbnail: Option<ThumbnailView>,
}

/// `MiniChatMessageDto` read model.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRole,
    pub content: String,
    /// Always present, possibly empty.
    pub attachments: Vec<AttachmentSummary>,
    /// Always present, possibly `None`.
    pub my_reaction: Option<Reaction>,
    pub created_at: OffsetDateTime,
    /// Only when `messages.model` is set.
    pub model: Option<String>,
    /// Only when the stored value is not 0.
    pub input_tokens: Option<i64>,
    /// Only when the stored value is not 0.
    pub output_tokens: Option<i64>,
}

/// The message projection of one page (`cpt-cf-mini-chat-algo-message-streaming-message-projection`):
/// `rows` in page order, the page's attachment batch and the caller's reactions by message id.
///
/// # Errors
///
/// `Internal` when a row has a NULL `request_id` (a message is never serialized without it).
pub fn project_messages(
    rows: Vec<MessageRow>,
    attachments: &[AttachmentRow],
    reactions: &BTreeMap<Uuid, Reaction>,
) -> Result<Vec<MessageView>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-null-request-id
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-return-500
    // a message is never serialized without its `request_id`: the whole page fails with a
    // generic detail and the row id goes to the log
    if let Some(row) = rows.iter().find(|row| row.request_id.is_none()) {
        tracing::error!(message_id = %row.id, "a stored message has no request_id");
        return Err(DomainError::internal(
            "a stored message has a NULL request_id",
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-return-500
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-null-request-id
    let mut views = Vec::with_capacity(rows.len());
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-each
    for row in rows {
        let Some(request_id) = row.request_id else {
            continue;
        };
        let role = MessageRole::parse(&row.role).ok_or_else(|| {
            DomainError::internal(format!("message {} has an unknown role", row.id))
        })?;
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-attachment-list
        // only a user message carries attachments; the summaries keep the order of the batch
        let summaries = if role == MessageRole::User {
            attachments
                .iter()
                .filter(|attachment| attachment.message_id == row.id)
                .map(summarize)
                .collect()
        } else {
            Vec::new()
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-attachment-list
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-reaction
        // only an assistant message carries the caller's reaction; the field is always present
        let my_reaction = if role == MessageRole::Assistant {
            reactions.get(&row.id).copied()
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-reaction
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-required
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-optional
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-no-internal
        // `model` only when set, the token counts only when not 0; no tenant, chat, deletion,
        // compression, provider or estimate field exists in the read model, and compressed
        // messages stay listed
        views.push(MessageView {
            id: row.id,
            request_id,
            role,
            content: row.content,
            attachments: summaries,
            my_reaction,
            created_at: row.created_at,
            model: row.model,
            input_tokens: (row.input_tokens != 0).then_some(row.input_tokens),
            output_tokens: (row.output_tokens != 0).then_some(row.output_tokens),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-no-internal
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-optional
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-required
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-each
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-return
    Ok(views)
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-return
}

/// The `AttachmentSummary` of a batch row: the thumbnail only for an image with status `ready`
/// and a stored thumbnail.
fn summarize(row: &AttachmentRow) -> AttachmentSummary {
    let thumbnail = if row.kind == "image" && row.status == "ready" {
        row.thumbnail.as_ref().map(|thumbnail| ThumbnailView {
            content_type: "image/webp".to_owned(),
            width: thumbnail.width,
            height: thumbnail.height,
            data: thumbnail.data.clone(),
        })
    } else {
        None
    };
    AttachmentSummary {
        attachment_id: row.attachment_id,
        kind: row.kind.clone(),
        filename: row.filename.clone(),
        status: row.status.clone(),
        img_thumbnail: thumbnail,
    }
}

/// Port of the two read paths, implemented by the infrastructure layer over the database
/// provider. Both reach their rows only through the owner-scoped chat of `scope`.
#[async_trait]
pub trait MessageStore: Send + Sync {
    /// One page of the live messages of the chat for the validated `OData` query, projected
    /// with the attachments and the reactions of `user_id`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat,
    /// [`DomainError::ODataQuery`] for a rejected filter, order or cursor, `Internal` for a
    /// NULL `request_id` or a database failure.
    async fn list_messages(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError>;

    /// The non-deleted turn `(chat_id, request_id)` of the chat of `scope`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the chat is not visible, `NotFound { resource: Turn }`
    /// for an unknown or soft-deleted turn.
    async fn load_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnRow, DomainError>;
}

/// The service behind the messages list and the turn status endpoints.
#[domain_model]
pub struct MessagesService {
    pub pep: Arc<PepService>,
    pub store: Arc<dyn MessageStore>,
}

impl MessagesService {
    /// Create the service; the database provider is the persistence port.
    #[must_use]
    pub fn new(pep: Arc<PepService>, db: Arc<DBProvider<DomainError>>) -> Self {
        Self { pep, store: db }
    }

    /// `GET /v1/chats/{id}/messages`: admission with the action `list_messages`, then one page.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { resource: Chat }`, [`DomainError::ODataQuery`],
    /// `Internal` for a NULL `request_id` or a database failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-admission
        let admission = self
            .pep
            .authorize(ctx, Operation::ListMessages, Some(chat_id))
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-refusal
        // a refusal, or the not-found of the owner-scoped chat load, is returned as it is
        let scope = scope_of(admission?)?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-200
        // the page, its items and its page info (effective limit, opaque cursors)
        self.store
            .list_messages(&scope, ctx.subject_id(), chat_id, query)
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-200
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`: admission with the action `read_turn`, then the
    /// status projection; the read changes nothing.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { resource: Chat }` for a chat that is not visible,
    /// `NotFound { resource: Turn }` for an unknown or soft-deleted turn.
    pub async fn turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-admission
        let admission = self
            .pep
            .authorize(ctx, Operation::ReadTurn, Some(chat_id))
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-refusal
        let scope = scope_of(admission?)?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-select-turn
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-not-found
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-404
        // an unknown or soft-deleted turn is the not-found of the turn type
        let row = self.store.load_turn(&scope, chat_id, request_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-not-found
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-select-turn
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-project
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-ui-action
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-200
        // the read changes nothing
        Ok(project_turn_status(&row))
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-return-200
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-ui-action
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-project
    }
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod messages_tests;
