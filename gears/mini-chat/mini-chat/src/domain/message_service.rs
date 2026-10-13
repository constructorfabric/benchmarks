//! Message history, reactions and turn status (DESIGN section 3.3 "List
//! Messages", "Message Reaction API", "Turn Status API"; section 3.7
//! `messages` / `message_attachments`).
//!
//! Every operation authorizes its action on the parent chat and loads the chat
//! through the scoped chat query first: an unknown, deleted or foreign chat is
//! `404 not_found` (chat) before any child row is read.
//!
//! - **List messages** — non-deleted messages, `created_at asc` by default;
//!   the page is enriched with two batch queries (attachment summaries of the
//!   page's messages, the caller's reactions on its assistant messages). A
//!   stored message without `request_id` fails the request (internal error).
//! - **Reactions** — the value is validated before authorization; only
//!   assistant messages accept a reaction (`failed_precondition`
//!   `reaction_target` / `STATE` otherwise, for `PUT` and `DELETE`); `PUT`
//!   upserts by `(message_id, user_id)`, `DELETE` is idempotent.
//! - **Turn status** — the non-deleted turn of `(chat_id, request_id)` with
//!   the internal-to-API state mapping `running → running`,
//!   `completed → done`, `failed → error`, `cancelled → cancelled`.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Action, AuthzService};
use crate::domain::chat_service::chat_not_found;
use crate::domain::error::{DomainError, InvalidArgumentReason, PreconditionSubject, ResourceKind};
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, AttachmentSummaryRecord, MessageRecord, MessageRole,
    NewReaction, Reaction, Repositories, TurnRecord, TurnState, storage_now,
};

/// MIME type of every stored thumbnail.
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// Preview thumbnail of a ready image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgThumbnail {
    /// Always [`THUMBNAIL_CONTENT_TYPE`].
    pub content_type: &'static str,
    pub width: i32,
    pub height: i32,
    /// Raw WebP bytes (the wire carries them base64-encoded).
    pub data: Vec<u8>,
}

/// `AttachmentSummary` embedded in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: AttachmentKind,
    pub filename: String,
    pub status: AttachmentStatus,
    /// Only for `kind = image` with `status = ready` and a stored thumbnail.
    pub img_thumbnail: Option<ImgThumbnail>,
}

impl AttachmentSummary {
    fn from_record(record: AttachmentSummaryRecord) -> Self {
        let img_thumbnail = match (record.kind, record.status) {
            (AttachmentKind::Image, AttachmentStatus::Ready) => {
                record.thumbnail.map(|thumbnail| ImgThumbnail {
                    content_type: THUMBNAIL_CONTENT_TYPE,
                    width: thumbnail.width,
                    height: thumbnail.height,
                    data: thumbnail.data,
                })
            }
            _ => None,
        };
        Self {
            attachment_id: record.attachment_id,
            kind: record.kind,
            filename: record.filename,
            status: record.status,
            img_thumbnail,
        }
    }
}

/// `Message` of the message history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    /// Always present (turn correlation key).
    pub request_id: Uuid,
    pub role: MessageRole,
    pub content: String,
    /// Effective model of an assistant message; `None` for user messages.
    pub model: Option<String>,
    /// Provider-reported counts; the wire omits a 0.
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Live attachments linked to the message (empty when none).
    pub attachments: Vec<AttachmentSummary>,
    /// The caller's reaction; always `None` for user and system messages.
    pub my_reaction: Option<Reaction>,
    pub created_at: OffsetDateTime,
}

/// The caller's reaction on a message (`PUT .../reaction` response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: Reaction,
    pub created_at: OffsetDateTime,
}

/// API turn state of the Turn Status API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

impl TurnStatusState {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// Internal-to-API state mapping.
    #[must_use]
    pub const fn from_internal(state: TurnState) -> Self {
        match state {
            TurnState::Running => Self::Running,
            TurnState::Completed => Self::Done,
            TurnState::Failed => Self::Error,
            TurnState::Cancelled => Self::Cancelled,
        }
    }
}

/// `GET /v1/chats/{id}/turns/{request_id}` response (no `chat_id`, no
/// billing or provider internals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    /// Terminal error code; present only for `error` (and only when stored).
    pub error_code: Option<String>,
    /// Persisted assistant message; present only for `done` and `cancelled`
    /// (a cancelled turn has one only when partial content was persisted),
    /// never while `running` or on `error`.
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

impl TurnStatus {
    /// The API view of a stored turn: the stored `error_code` and
    /// `assistant_message_id` columns are exposed only in the states DESIGN
    /// "Turn Status API" allows them in, whatever else is stored.
    fn from_record(record: TurnRecord) -> Self {
        let state = TurnStatusState::from_internal(record.state);
        let error_code = record
            .error_code
            .filter(|_| state == TurnStatusState::Error);
        let assistant_message_id = record
            .assistant_message_id
            .filter(|_| matches!(state, TurnStatusState::Done | TurnStatusState::Cancelled));
        Self {
            request_id: record.request_id,
            state,
            error_code,
            assistant_message_id,
            updated_at: record.updated_at,
        }
    }
}

/// Message history, reaction and turn status domain service.
pub struct MessageService {
    repos: Repositories,
    authz: std::sync::Arc<AuthzService>,
}

impl std::fmt::Debug for MessageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageService")
            .field("authz", &self.authz)
            .finish_non_exhaustive()
    }
}

impl MessageService {
    /// Builds the service over the persistence ports and the gear PEP.
    #[must_use]
    pub fn new(repos: Repositories, authz: std::sync::Arc<AuthzService>) -> Self {
        Self { repos, authz }
    }

    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    ///
    /// PEP errors of `list_messages`; `404` (chat);
    /// [`DomainError::InvalidQuery`] for an invalid `OData` query;
    /// [`DomainError::Internal`] for a stored message without `request_id`.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        if query.limit == Some(0) {
            return Err(DomainError::InvalidQuery(
                toolkit_odata::Error::InvalidLimit,
            ));
        }
        let scope = self
            .authz
            .authorize_chat(ctx, Action::ListMessages, Some(chat_id))
            .await?;
        self.repos
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;

        let page = self
            .repos
            .messages
            .list_active(&scope, chat_id, query)
            .await?;
        // Implementation guard: a message is never served without request_id.
        if let Some(message) = page.items.iter().find(|m| m.request_id.is_none()) {
            return Err(missing_request_id(message.id));
        }

        let message_ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut attachments = self
            .attachments_by_message(&scope, chat_id, &message_ids)
            .await?;
        let reactions = self.reactions_by_message(ctx, &scope, &page.items).await?;

        let items = page
            .items
            .into_iter()
            .map(|message| {
                let request_id = message
                    .request_id
                    .ok_or_else(|| missing_request_id(message.id))?;
                Ok(MessageView {
                    id: message.id,
                    request_id,
                    role: message.role,
                    content: message.content,
                    model: message.model,
                    input_tokens: message.input_tokens,
                    output_tokens: message.output_tokens,
                    attachments: attachments.remove(&message.id).unwrap_or_default(),
                    my_reaction: reactions.get(&message.id).copied(),
                    created_at: message.created_at,
                })
            })
            .collect::<Result<Vec<_>, DomainError>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    ///
    /// # Errors
    ///
    /// `INVALID_REACTION` (checked before authorization), PEP errors of
    /// `set_reaction`, `404` (chat, then message), `failed_precondition`
    /// `reaction_target` for a non-assistant message.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let reaction = Reaction::parse(reaction)
            .ok_or_else(|| DomainError::invalid_argument(InvalidArgumentReason::InvalidReaction))?;
        let scope = self
            .authz
            .authorize_chat(ctx, Action::SetReaction, Some(chat_id))
            .await?;
        self.reaction_target(&scope, chat_id, message_id).await?;
        let stored = self
            .repos
            .reactions
            .upsert(
                &scope,
                NewReaction {
                    id: Uuid::new_v4(),
                    message_id,
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    reaction,
                    created_at: storage_now(),
                },
            )
            .await?;
        Ok(ReactionView {
            message_id: stored.message_id,
            reaction: stored.reaction,
            created_at: stored.created_at,
        })
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    ///
    /// PEP errors of `delete_reaction`, `404` (chat, then message),
    /// `failed_precondition` `reaction_target` for a non-assistant message.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .authz
            .authorize_chat(ctx, Action::DeleteReaction, Some(chat_id))
            .await?;
        self.reaction_target(&scope, chat_id, message_id).await?;
        self.repos
            .reactions
            .delete(&scope, message_id, ctx.subject_id())
            .await
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// PEP errors of `read_turn`; `404` (chat), then `404` (turn) when no
    /// non-deleted turn of the chat has `request_id`.
    pub async fn get_turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatus, DomainError> {
        let scope = self
            .authz
            .authorize_chat(ctx, Action::ReadTurn, Some(chat_id))
            .await?;
        self.repos
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let turn = self
            .repos
            .turns
            .find_active_by_request(&scope, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::not_found(ResourceKind::Turn, request_id.to_string()))?;
        Ok(TurnStatus::from_record(turn))
    }

    /// Checks the chat, the message and its role for a reaction change.
    async fn reaction_target(
        &self,
        scope: &toolkit_security::AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<MessageRecord, DomainError> {
        self.repos
            .chats
            .find_active(scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let message = self
            .repos
            .messages
            .find_active(scope, chat_id, message_id)
            .await?
            .ok_or_else(|| DomainError::not_found(ResourceKind::Message, message_id.to_string()))?;
        if message.role != MessageRole::Assistant {
            return Err(DomainError::failed_precondition(
                PreconditionSubject::ReactionTarget,
            ));
        }
        Ok(message)
    }

    async fn attachments_by_message(
        &self,
        scope: &toolkit_security::AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentSummary>>, DomainError> {
        let mut by_message: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        if message_ids.is_empty() {
            return Ok(by_message);
        }
        let records = self
            .repos
            .attachments
            .summaries_for_messages(scope, chat_id, message_ids)
            .await?;
        for record in records {
            by_message
                .entry(record.message_id)
                .or_default()
                .push(AttachmentSummary::from_record(record));
        }
        Ok(by_message)
    }

    /// The caller's reactions on the page's assistant messages.
    async fn reactions_by_message(
        &self,
        ctx: &SecurityContext,
        scope: &toolkit_security::AccessScope,
        messages: &[MessageRecord],
    ) -> Result<HashMap<Uuid, Reaction>, DomainError> {
        let assistant_ids: Vec<Uuid> = messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .map(|m| m.id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if assistant_ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.repos
            .reactions
            .reactions_of_user(scope, ctx.subject_id(), &assistant_ids)
            .await
    }
}

/// Internal error of a stored message without `request_id` (the diagnostic
/// stays server-side).
fn missing_request_id(message_id: Uuid) -> DomainError {
    DomainError::internal(format!("stored message {message_id} has no request_id"))
}

#[cfg(test)]
#[path = "message_service_tests.rs"]
mod tests;
