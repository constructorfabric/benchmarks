//! Messages list (D§3.3 "List Messages", S§5.1).

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz;
use crate::domain::error::DomainError;
use crate::domain::models::{AttachmentSummary, MessageView, ready_image_thumbnail};
use crate::domain::services::ChatService;
use crate::infra::db::entity::message;
use crate::infra::db::repos::{AttachmentRepo, MessageAttachmentRepo, MessageRepo, ReactionRepo};

/// Paginated, enriched message history of a chat.
pub struct MessageService {
    db: Arc<DBProvider<DomainError>>,
    chats: Arc<ChatService>,
}

impl MessageService {
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, chats: Arc<ChatService>) -> Self {
        Self { db, chats }
    }

    /// One page of the chat's non-deleted messages (default order
    /// `created_at asc, id asc`), each with its non-deleted attachments and
    /// the caller's reaction.
    ///
    /// # Errors
    /// `ChatNotFound`, `InvalidQuery`, authorization and database failures;
    /// `Internal` for a stored message without `request_id`.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::LIST_MESSAGES, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let page = MessageRepo
            .list_page(&conn, &scope, chat.id, query, self.db.db().backend())
            .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();

        let links = MessageAttachmentRepo
            .list_for_messages(&conn, &scope, chat.id, &ids)
            .await?;
        let attachment_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let attachments: HashMap<Uuid, AttachmentSummary> = AttachmentRepo
            .find_in_chat(&conn, &scope, chat.id, &attachment_ids)
            .await?
            .into_iter()
            .filter(|a| a.deleted_at.is_none())
            .map(|a| {
                let thumbnail = ready_image_thumbnail(&a);
                (
                    a.id,
                    AttachmentSummary {
                        attachment_id: a.id,
                        kind: a.attachment_kind,
                        filename: a.filename,
                        status: a.status,
                        thumbnail,
                    },
                )
            })
            .collect();
        let mut by_message: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        for l in &links {
            if let Some(a) = attachments.get(&l.attachment_id) {
                by_message.entry(l.message_id).or_default().push(a.clone());
            }
        }
        let reactions: HashMap<Uuid, String> = ReactionRepo
            .list_for_messages(&conn, &scope, ctx.subject_id(), &ids)
            .await?
            .into_iter()
            .map(|r| (r.message_id, r.reaction))
            .collect();

        let mut items = Vec::with_capacity(page.items.len());
        for m in &page.items {
            items.push(view(m, &mut by_message, &reactions)?);
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}

fn view(
    m: &message::Model,
    attachments: &mut HashMap<Uuid, Vec<AttachmentSummary>>,
    reactions: &HashMap<Uuid, String>,
) -> Result<MessageView, DomainError> {
    // API invariant: every exposed message has a request id.
    let request_id = m
        .request_id
        .ok_or_else(|| DomainError::Internal(format!("message {} has no request_id", m.id)))?;
    let my_reaction = if m.role == "assistant" {
        reactions.get(&m.id).cloned()
    } else {
        None
    };
    Ok(MessageView {
        id: m.id,
        request_id,
        role: m.role.clone(),
        content: m.content.clone(),
        attachments: attachments.remove(&m.id).unwrap_or_default(),
        my_reaction,
        model: m.model.clone(),
        input_tokens: m.input_tokens,
        output_tokens: m.output_tokens,
        created_at: m.created_at,
    })
}
