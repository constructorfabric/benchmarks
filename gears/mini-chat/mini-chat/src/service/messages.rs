//! Message history listing and turn status.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use crate::api::rest::odata::MessageQueryFieldsFilterField;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entity::{
    attachment, chat_turn, message, message_attachment, message_reaction,
};
use crate::infra::odata::MessageODataMapper;
use crate::infra::repo;

/// Attachment summary embedded in messages.
#[derive(Debug, Clone)]
pub struct AttachmentSummary {
    pub attachment: attachment::Model,
}

#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: message::Model,
    pub request_id: Uuid,
    pub attachments: Vec<attachment::Model>,
    pub my_reaction: Option<String>,
}

/// Non-deleted attachments linked to the given messages, keyed by message id.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn attachments_for_messages(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<HashMap<Uuid, Vec<attachment::Model>>> {
    let mut out: HashMap<Uuid, Vec<attachment::Model>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let links = message_attachment::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .all(r)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts = attachment::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(att_ids))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .all(r)
        .await?;
    let by_id: HashMap<Uuid, attachment::Model> = atts.into_iter().map(|a| (a.id, a)).collect();
    let mut links = links;
    links.sort_by_key(|l| (l.created_at, l.attachment_id));
    for l in links {
        if let Some(a) = by_id.get(&l.attachment_id) {
            out.entry(l.message_id).or_default().push(a.clone());
        }
    }
    Ok(out)
}

/// Turn status as exposed by the API.
#[derive(Debug, Clone)]
pub struct TurnStatusView {
    pub turn: chat_turn::Model,
}

impl AppState {
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> DomainResult<Page<MessageView>> {
        let scopes =
            authz::chat_scopes(&self.enforcer, ctx, actions::LIST_MESSAGES, Some(chat_id)).await?;
        let conn = self.conn()?;
        repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let mut q = query.clone();
        if q.order.0.is_empty() && q.cursor.is_none() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let select = message::Entity::find()
            .secure()
            .scope_with(&scopes.tenant)
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            );
        let page = paginate_odata::<MessageQueryFieldsFilterField, MessageODataMapper, _, _, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Asc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts = attachments_for_messages(&conn, &scopes.tenant, chat_id, &ids).await?;
        let reactions: HashMap<Uuid, String> = if ids.is_empty() {
            HashMap::new()
        } else {
            message_reaction::Entity::find()
                .secure()
                .scope_with(&scopes.tenant)
                .filter(
                    Condition::all()
                        .add(message_reaction::Column::MessageId.is_in(ids.clone()))
                        .add(message_reaction::Column::UserId.eq(ctx.subject_id())),
                )
                .all(&conn)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect()
        };
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let request_id = m.request_id.ok_or_else(|| {
                DomainError::internal(format!("message {} has no request_id", m.id))
            })?;
            let my_reaction = if m.role == "assistant" {
                reactions.get(&m.id).cloned()
            } else {
                None
            };
            let attachments = atts.remove(&m.id).unwrap_or_default();
            items.push(MessageView {
                message: m,
                request_id,
                attachments,
                my_reaction,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn get_turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<TurnStatusView> {
        let scopes =
            authz::chat_scopes(&self.enforcer, ctx, actions::READ_TURN, Some(chat_id)).await?;
        let conn = self.conn()?;
        repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let turn = repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or_else(|| DomainError::not_found(Res::Turn, request_id))?;
        Ok(TurnStatusView { turn })
    }
}
