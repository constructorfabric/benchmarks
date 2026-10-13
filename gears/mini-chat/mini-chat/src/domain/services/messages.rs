//! Message listing with attachment and reaction enrichment.

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::models::attachment_kind;
use crate::infra::db::entities::{attachments, messages};
use crate::infra::db::odata::{MessageMapper, MessageQueryFieldsFilterField};
use crate::infra::db::repo;

/// Thumbnail of an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// Attachment summary embedded in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub thumbnail: Option<ThumbnailView>,
}

/// Thumbnail of a row (images in `ready` only).
#[must_use]
pub fn thumbnail_of(a: &attachments::Model) -> Option<ThumbnailView> {
    if a.attachment_kind != attachment_kind::IMAGE || a.status != "ready" {
        return None;
    }
    Some(ThumbnailView {
        data: a.img_thumbnail.clone()?,
        width: a.img_thumbnail_width.unwrap_or_default(),
        height: a.img_thumbnail_height.unwrap_or_default(),
    })
}

/// Message with enrichment.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: messages::Model,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<String>,
}

impl MiniChatService {
    /// Lists messages (default `created_at asc, id asc`).
    ///
    /// # Errors
    /// 404, `OData` errors, authorization errors.
    pub async fn list_messages(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::LIST_MESSAGES, chat_id).await?;
        let mut q = query.clone();
        if q.order.is_empty() && q.cursor.is_none() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let conn = self.db.conn()?;
        let page = paginate_odata::<MessageQueryFieldsFilterField, MessageMapper, _, _, _, _>(
            repo::messages::list_select(chat.tenant_id, chat.id),
            &conn,
            &q,
            ("id", SortDir::Asc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let links = repo::messages::links_for_messages(&conn, chat.tenant_id, chat.id, &ids).await?;
        let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: HashMap<Uuid, attachments::Model> =
            repo::attachments::find_many(&conn, chat.tenant_id, chat.id, &att_ids)
                .await?
                .into_iter()
                .filter(|a| a.deleted_at.is_none())
                .map(|a| (a.id, a))
                .collect();
        let reactions: HashMap<Uuid, String> =
            repo::reactions::for_messages(&conn, chat.tenant_id, ctx.subject_id(), &ids)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect();
        let mut by_msg: HashMap<Uuid, Vec<AttachmentSummaryView>> = HashMap::new();
        for l in &links {
            if let Some(a) = atts.get(&l.attachment_id) {
                by_msg.entry(l.message_id).or_default().push(AttachmentSummaryView {
                    attachment_id: a.id,
                    kind: a.attachment_kind.clone(),
                    filename: a.filename.clone(),
                    status: a.status.clone(),
                    thumbnail: thumbnail_of(a),
                });
            }
        }
        let items = page
            .items
            .into_iter()
            .map(|m| {
                if m.request_id.is_none() {
                    return Err(DomainError::internal("stored message without request_id"));
                }
                let my_reaction = if m.role == "assistant" {
                    reactions.get(&m.id).cloned()
                } else {
                    None
                };
                let attachments = by_msg.remove(&m.id).unwrap_or_default();
                Ok(MessageView {
                    message: m,
                    attachments,
                    my_reaction,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}
