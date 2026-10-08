//! Message history, reactions and turn status.

use std::collections::HashMap;

use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz::actions;
use crate::domain::chats::ListError;
use crate::domain::error::{DomainError, DomainResult, Resource};
use crate::domain::time::now;
use crate::infra::db::entities::{attachment, chat_turn, message};
use crate::infra::db::odata::{MessageFilterField, MessageMapper};
use crate::infra::db::repo;

/// A message with its attachments and the caller's reaction.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: message::Model,
    pub attachments: Vec<attachment::Model>,
    pub my_reaction: Option<String>,
}

/// Validated reaction value.
///
/// # Errors
/// `InvalidReaction`.
pub fn validate_reaction(raw: &str) -> DomainResult<&'static str> {
    match raw {
        "like" => Ok("like"),
        "dislike" => Ok("dislike"),
        _ => Err(DomainError::InvalidReaction),
    }
}

impl AppServices {
    /// Lists messages: default order `created_at asc`, `id` tiebreaker.
    ///
    /// # Errors
    /// `NotFound(Chat)`, `OData` errors, authorization errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, ListError> {
        use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
        use toolkit_db::secure::SecureEntityExt;
        let chat = self
            .authorized_chat(ctx, actions::LIST_MESSAGES, chat_id)
            .await?;
        let conn = self.db.conn().map_err(ListError::Domain)?;
        let mut query = query.clone();
        if query.order.0.is_empty() && query.cursor.is_none() {
            query.order = toolkit_odata::ODataOrderBy(vec![toolkit_odata::OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let select = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat.id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&crate::domain::authz::child_scope(chat.tenant_id));
        let page = toolkit_db::odata::paginate_odata::<
            MessageFilterField,
            MessageMapper,
            message::Entity,
            message::Model,
            _,
            _,
        >(
            select,
            &conn,
            &query,
            ("id", SortDir::Asc),
            toolkit_db::odata::LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await
        .map_err(ListError::OData)?;
        if page.items.iter().any(|m| m.request_id.is_none()) {
            return Err(ListError::Domain(DomainError::internal(
                "stored message without request_id",
            )));
        }
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let links = repo::message_attachment_links(&conn, chat.tenant_id, chat.id, &ids)
            .await
            .map_err(ListError::Domain)?;
        let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: HashMap<Uuid, attachment::Model> =
            repo::attachments_by_ids(&conn, chat.tenant_id, chat.id, &att_ids)
                .await
                .map_err(ListError::Domain)?
                .into_iter()
                .map(|a| (a.id, a))
                .collect();
        let reactions = repo::reactions_for(&conn, chat.tenant_id, ctx.subject_id(), &ids)
            .await
            .map_err(ListError::Domain)?;
        let mut per_msg: HashMap<Uuid, Vec<attachment::Model>> = HashMap::new();
        for l in links {
            if let Some(a) = atts.get(&l.attachment_id) {
                per_msg.entry(l.message_id).or_default().push(a.clone());
            }
        }
        Ok(page.map_items(|m| {
            let attachments = per_msg.remove(&m.id).unwrap_or_default();
            let my_reaction = if m.role == "assistant" {
                reactions.get(&m.id).cloned()
            } else {
                None
            };
            MessageView {
                message: m,
                attachments,
                my_reaction,
            }
        }))
    }

    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<message::Model> {
        let chat = self.authorized_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = repo::find_message(&conn, chat.tenant_id, chat.id, msg_id)
            .await?
            .ok_or(DomainError::NotFound(Resource::Message))?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTarget);
        }
        Ok(msg)
    }

    /// Sets (upserts) the caller's reaction.
    ///
    /// # Errors
    /// `InvalidReaction` (checked first), `NotFound`, `ReactionTarget`.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> DomainResult<(Uuid, &'static str, time::OffsetDateTime)> {
        let reaction = validate_reaction(reaction)?;
        let msg = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        let ts = now();
        repo::upsert_reaction(&conn, msg.tenant_id, ctx.subject_id(), msg.id, reaction, ts).await?;
        let stored = repo::find_reaction(&conn, msg.tenant_id, ctx.subject_id(), msg.id).await?;
        Ok((msg.id, reaction, stored.map_or(ts, |r| r.created_at)))
    }

    /// Removes the caller's reaction (idempotent).
    ///
    /// # Errors
    /// `NotFound`, `ReactionTarget`.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<()> {
        let msg = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        repo::delete_reaction(&conn, msg.tenant_id, ctx.subject_id(), msg.id).await?;
        Ok(())
    }

    /// Authoritative turn status; soft-deleted turns are not found.
    ///
    /// # Errors
    /// `NotFound(Chat|Turn)`.
    pub async fn get_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<chat_turn::Model> {
        let chat = self
            .authorized_chat(ctx, actions::READ_TURN, chat_id)
            .await?;
        let conn = self.db.conn()?;
        repo::find_turn_by_request(&conn, chat.tenant_id, chat.id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::NotFound(Resource::Turn))
    }
}
