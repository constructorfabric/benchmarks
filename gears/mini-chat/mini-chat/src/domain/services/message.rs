//! Messages list (DESIGN §3.3 "List Messages").

use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::ListError;
use super::chat::ChatService;
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::error::DomainError;
use crate::domain::model::MessageRole;
use crate::infra::db::entities::message;
use crate::infra::db::repos::attachment::AttachmentSummary;
use crate::infra::db::repos::{AttachmentRepo, MessageRepo, ReactionRepo};

/// A listed message with its attachments and the caller's reaction.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: message::Model,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<String>,
}

pub struct MessageService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    chats: Arc<ChatService>,
}

impl MessageService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<ChatAuthz>,
        chats: Arc<ChatService>,
    ) -> Self {
        Self { db, authz, chats }
    }

    /// One page of the chat's non-deleted messages. Attachments and the caller's
    /// reactions are loaded with one batch query each per page.
    ///
    /// # Errors
    /// `ListError::OData` for filter/order/cursor errors; `ListError::Domain` for
    /// authorization errors, `ChatNotFound`, database failures and a stored
    /// message without `request_id` (`Internal`, never serialized as null).
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, ListError> {
        let scope = self
            .authz
            .chat_scope(ctx, actions::LIST_MESSAGES, Some(chat_id))
            .await?;
        let chat = self.chats.load_chat(&scope, chat_id).await?;
        let conn = self.db.conn()?;
        let page = MessageRepo::list_page(&conn, chat.tenant_id, chat.id, query).await?;

        if let Some(m) = page.items.iter().find(|m| m.request_id.is_none()) {
            return Err(
                DomainError::internal(format!("message {} has no request_id", m.id)).into(),
            );
        }

        let all_ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        // Only assistant messages carry reactions.
        let assistant_ids: Vec<Uuid> = page
            .items
            .iter()
            .filter(|m| m.role == MessageRole::Assistant.as_str())
            .map(|m| m.id)
            .collect();
        let mut attachments =
            AttachmentRepo::summaries_for_messages(&conn, chat.tenant_id, chat.id, &all_ids)
                .await?;
        let mut reactions =
            ReactionRepo::for_messages(&conn, chat.tenant_id, ctx.subject_id(), &assistant_ids)
                .await?;

        let items = page
            .items
            .into_iter()
            .map(|message| {
                let attachments = attachments.remove(&message.id).unwrap_or_default();
                let my_reaction = reactions.remove(&message.id);
                MessageView {
                    message,
                    attachments,
                    my_reaction,
                }
            })
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}
