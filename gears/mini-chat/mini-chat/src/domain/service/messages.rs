//! Messages service: the message history page with its response contract and
//! the like/dislike reactions on assistant messages
//! (`cpt-cf-mini-chat-feature-chat-crud`, slices 5 and 6).

use std::collections::HashMap;
use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{AuthzService, ChatAction};
use crate::domain::clock::now_micros;
use crate::domain::error::DomainError;
use crate::domain::model::Message;
use crate::domain::ports::{AttachmentRow, MessageRepository, MessageRow};

/// A reaction value.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// `like`
    Like,
    /// `dislike`
    Dislike,
}

impl Reaction {
    /// Wire and storage value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }

    /// Parses the exact, case-sensitive wire value
    /// (`cpt-cf-mini-chat-flow-chat-crud-set-reaction`, step 2).
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidReaction`] for anything but `like` and `dislike`.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "like" => Ok(Self::Like),
            "dislike" => Ok(Self::Dislike),
            _ => Err(DomainError::InvalidReaction),
        }
    }
}

/// Thumbnail of a ready image attachment (stored as WebP).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgThumbnail {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// WebP bytes.
    pub data: Vec<u8>,
}

/// Attachment of a message as listed in the history.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// File name.
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// Only for a `ready` image with stored thumbnail bytes.
    pub img_thumbnail: Option<ImgThumbnail>,
}

/// A message of the history page.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Message id.
    pub id: Uuid,
    /// Request id (never null).
    pub request_id: Uuid,
    /// `user`, `assistant` or `system`.
    pub role: String,
    /// Content.
    pub content: String,
    /// Non-deleted attachments.
    pub attachments: Vec<AttachmentSummary>,
    /// The caller's reaction on an assistant message.
    pub my_reaction: Option<Reaction>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Effective model, when stored.
    pub model: Option<String>,
    /// Input tokens, when not 0.
    pub input_tokens: Option<i64>,
    /// Output tokens, when not 0.
    pub output_tokens: Option<i64>,
}

/// The stored reaction returned by `PUT .../reaction`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    /// Reacted message.
    pub message_id: Uuid,
    /// Stored value.
    pub reaction: Reaction,
    /// Time of the `PUT`.
    pub created_at: OffsetDateTime,
}

/// Builds the summary of one attachment: the thumbnail only for a `ready`
/// image with stored thumbnail bytes.
fn summarize(row: AttachmentRow) -> AttachmentSummary {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-build-attachment-summary
    let ready_image = row.attachment_kind == "image" && row.status == "ready";
    let img_thumbnail = match (ready_image, row.img_thumbnail) {
        (true, Some(data)) => Some(ImgThumbnail {
            width: row.img_thumbnail_width.unwrap_or(0),
            height: row.img_thumbnail_height.unwrap_or(0),
            data,
        }),
        _ => None,
    };
    AttachmentSummary {
        attachment_id: row.id,
        kind: row.attachment_kind,
        filename: row.filename,
        status: row.status,
        img_thumbnail,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-build-attachment-summary
}

/// Message history and reactions behind the PEP.
pub struct MessagesService {
    authz: Arc<AuthzService>,
    messages: Arc<dyn MessageRepository>,
}

impl MessagesService {
    /// Creates the service.
    #[must_use]
    pub fn new(authz: Arc<AuthzService>, messages: Arc<dyn MessageRepository>) -> Self {
        Self { authz, messages }
    }

    /// Enriches one page of stored messages into the response contract
    /// (`cpt-cf-mini-chat-algo-chat-crud-enrich-messages`).
    async fn enrich(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat_id: Uuid,
        rows: Vec<MessageRow>,
    ) -> Result<Vec<MessageView>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-if-page-empty
        if rows.is_empty() {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-return-empty-page
            return Ok(Vec::new());
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-return-empty-page
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-if-page-empty

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-for-each-request-id
        let mut keyed: Vec<(Uuid, MessageRow)> = Vec::with_capacity(rows.len());
        for row in rows {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-guard-request-id
            let Some(request_id) = row.request_id else {
                tracing::error!(message_id = %row.id, %chat_id, "stored message has no request_id");
                return Err(DomainError::Internal(
                    "stored message without request_id".to_owned(),
                ));
            };
            keyed.push((request_id, row));
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-guard-request-id
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-for-each-request-id

        let message_ids: Vec<Uuid> = keyed.iter().map(|(_, row)| row.id).collect();
        let assistant_ids: Vec<Uuid> = keyed
            .iter()
            .filter(|(_, row)| row.role == "assistant")
            .map(|(_, row)| row.id)
            .collect();

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-attachments
        let mut attachments: HashMap<Uuid, Vec<AttachmentRow>> = self
            .messages
            .attachments_of(scope, chat_id, &message_ids)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-attachments

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-reactions
        let reactions: HashMap<Uuid, String> = self
            .messages
            .reactions_of(scope, ctx.subject_id(), &assistant_ids)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-reactions

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-for-each-message
        let mut views = Vec::with_capacity(keyed.len());
        for (request_id, row) in keyed {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-set-attachments
            let summaries = attachments
                .remove(&row.id)
                .unwrap_or_default()
                .into_iter()
                .map(summarize)
                .collect();
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-set-attachments

            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-if-has-reaction
            let stored = if row.role == "assistant" {
                reactions.get(&row.id)
            } else {
                None
            };
            let my_reaction = if let Some(value) = stored {
                // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-set-my-reaction
                Reaction::parse(value).ok()
                // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-set-my-reaction
            } else {
                // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-else-no-reaction
                // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-null-my-reaction
                None
                // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-null-my-reaction
                // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-else-no-reaction
            };
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-if-has-reaction

            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-project-message-core
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-optional-message-fields
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-message-non-exposure
            // Only these columns are projected: `tenant_id`, `chat_id`, the
            // provider identifiers, the estimate, cache and reasoning counts,
            // `is_compressed`, `deleted_at` and other users' reactions never
            // leave the repository.
            views.push(MessageView {
                id: row.id,
                request_id,
                role: row.role,
                content: row.content,
                attachments: summaries,
                my_reaction,
                created_at: row.created_at,
                model: row.model,
                input_tokens: Some(row.input_tokens).filter(|count| *count != 0),
                output_tokens: Some(row.output_tokens).filter(|count| *count != 0),
            });
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-message-non-exposure
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-optional-message-fields
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-project-message-core
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-for-each-message

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-return-messages
        Ok(views)
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-return-messages
    }

    /// Resolves the target of a reaction
    /// (`cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`).
    async fn resolve_reaction_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(crate::domain::model::Chat, Message), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-load-target
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-target-chat-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target-chat-404
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-target-message-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target-message-404
        // The scoped access reports `NotFound { Chat }` and
        // `NotFound { Message }` (404 with the matching resource type).
        let (chat, message) = self
            .messages
            .load_message(scope, chat_id, message_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target-message-404
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-target-message-missing
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target-chat-404
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-target-chat-missing
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-load-target

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-not-assistant
        if message.role != "assistant" {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-reaction-target
            return Err(DomainError::ReactionTargetNotAssistant);
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-reaction-target
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-if-not-assistant

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target
        Ok((chat, message))
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target:p1:inst-return-target
    }

    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }`, `InvalidQuery`, `AuthzDenied`, `PdpUnavailable`,
    /// `Internal` (stored NULL `request_id`), `Database`.
    // @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-messages-api:p1
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-authorize-list-messages
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::ListMessages, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-authorize-list-messages

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-load-chat-messages
        let chat = self.messages.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-load-chat-messages

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-page-messages
        let page = self.messages.page(&scope, chat.id, query).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-page-messages

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-enrich-page
        let items = self.enrich(&scope, ctx, chat.id, page.items).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-enrich-page

        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    ///
    /// # Errors
    ///
    /// `InvalidReaction`, `NotFound { Chat | Message }`,
    /// `ReactionTargetNotAssistant`, `AuthzDenied`, `PdpUnavailable`,
    /// `Database`.
    // @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-reactions:p1
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-if-bad-reaction
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-invalid-reaction
        let reaction = Reaction::parse(reaction)?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-invalid-reaction
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-if-bad-reaction

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-authorize-set-reaction
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::SetReaction, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-authorize-set-reaction

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-resolve-set-target
        let (chat, message) = self
            .resolve_reaction_target(&scope, chat_id, message_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-resolve-set-target

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-upsert-reaction
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-no-chat-bump
        // Only `message_reactions` is written: `chats.updated_at` and the
        // message row stay unchanged.
        let stored = self
            .messages
            .upsert_reaction(
                &scope,
                chat.tenant_id,
                ctx.subject_id(),
                message.id,
                reaction.as_str(),
                now_micros(),
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-no-chat-bump
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-upsert-reaction

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-reaction
        Ok(ReactionView {
            message_id: stored.message_id,
            reaction: Reaction::parse(&stored.reaction)?,
            created_at: stored.created_at,
        })
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-reaction
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    ///
    /// `NotFound { Chat | Message }`, `ReactionTargetNotAssistant`,
    /// `AuthzDenied`, `PdpUnavailable`, `Database`.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-authorize-delete-reaction
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::DeleteReaction, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-authorize-delete-reaction

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-resolve-remove-target
        let (_chat, message) = self
            .resolve_reaction_target(&scope, chat_id, message_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-resolve-remove-target

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-delete-reaction
        self.messages
            .delete_reaction(&scope, ctx.subject_id(), message.id)
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-delete-reaction
    }
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod tests;
