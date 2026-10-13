//! Message listing (DESIGN §3.3 "List Messages").

use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::odata::paginate_odata;
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::chat_service::LIMIT_CFG;
use crate::domain::error::DomainError;
use crate::domain::service::{Svc, child_scope};
use crate::infra::db::entities::{attachments, message_reactions, messages};
use crate::infra::db::odata::{MessageField, MessageMapper};

/// Message with its enrichments.
#[derive(Debug, Clone)]
pub struct MessageView {
    /// Row.
    pub message: messages::Model,
    /// Non-deleted linked attachments.
    pub attachments: Vec<attachments::Model>,
    /// Caller's reaction.
    pub my_reaction: Option<String>,
}

/// Loads linked attachments and reactions for a page of messages.
///
/// # Errors
/// Database errors.
pub async fn enrich(
    runner: &impl DBRunner,
    scope: &AccessScope,
    user_id: Uuid,
    rows: Vec<messages::Model>,
) -> Result<Vec<MessageView>, DomainError> {
    let ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
    let links = crate::domain::repo::links_of_messages(runner, scope, &ids).await?;
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts: HashMap<Uuid, attachments::Model> = if att_ids.is_empty() {
        HashMap::new()
    } else {
        attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.is_in(att_ids))
                    .add(attachments::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(scope)
            .all(runner)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect()
    };
    let reactions: HashMap<Uuid, String> = if ids.is_empty() {
        HashMap::new()
    } else {
        message_reactions::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.is_in(ids.clone()))
                    .add(message_reactions::Column::UserId.eq(user_id)),
            )
            .secure()
            .scope_with(scope)
            .all(runner)
            .await?
            .into_iter()
            .map(|r| (r.message_id, r.reaction))
            .collect()
    };
    let mut by_msg: HashMap<Uuid, Vec<attachments::Model>> = HashMap::new();
    for l in &links {
        if let Some(a) = atts.get(&l.attachment_id) {
            by_msg.entry(l.message_id).or_default().push(a.clone());
        }
    }
    Ok(rows
        .into_iter()
        .map(|m| {
            let mut list = by_msg.remove(&m.id).unwrap_or_default();
            list.sort_by_key(|a| (a.created_at, a.id));
            let my_reaction = if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None };
            MessageView { message: m, attachments: list, my_reaction }
        })
        .collect())
}

impl Svc {
    /// `GET /chats/{id}/messages`.
    ///
    /// # Errors
    /// 404, `OData` 400, PDP errors.
    pub async fn list_messages(&self, ctx: &SecurityContext, chat_id: Uuid, query: &ODataQuery) -> Result<Page<MessageView>, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::LIST_MESSAGES, chat_id).await?;
        let scope = child_scope(&chat);
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy::empty().ensure_tiebreaker("created_at", SortDir::Asc);
        }
        let conn = self.db.conn()?;
        let select = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null())
                    .add(messages::Column::RequestId.is_not_null()),
            )
            .secure()
            .scope_with(&scope);
        let q = crate::infra::db::odata::sqlite_safe_query(&q, self.db.db().backend(), &["created_at"]);
        let page = paginate_odata::<MessageField, MessageMapper, messages::Entity, messages::Model, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Asc),
            LIMIT_CFG,
            |m| m,
        )
        .await?;
        let items = enrich(&conn, &scope, ctx.subject_id(), page.items).await?;
        Ok(Page { items, page_info: page.page_info })
    }
}
