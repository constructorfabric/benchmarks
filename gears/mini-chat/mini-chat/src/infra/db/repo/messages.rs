//! `MessageRepository` over the Secure ORM.

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::paginate_odata;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::limit_cfg;
use super::odata::MessageODataMapper;
use crate::domain::error::DomainError;
use crate::domain::messages::{
    AttachmentSummaryRow, MessageFilterField, MessageRow, NewMessage, SummaryFrontier,
};
use crate::domain::ports::{MessageMutationRepository, MessageRepository, RecentQuery};
use crate::infra::db::entity::{attachments, message_attachments, messages};
use crate::infra::db::timestamp::{from_db, to_db};

/// `(created_at, id) > frontier` in the strict total order of the messages.
pub(super) fn after_frontier(frontier: &SummaryFrontier) -> Condition {
    let at = to_db(frontier.created_at);
    Condition::any()
        .add(messages::Column::CreatedAt.gt(at))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(at))
                .add(messages::Column::Id.gt(frontier.message_id)),
        )
}

/// `(created_at, id) <= frontier`.
pub(super) fn up_to_frontier(frontier: &SummaryFrontier) -> Condition {
    let at = to_db(frontier.created_at);
    Condition::any()
        .add(messages::Column::CreatedAt.lt(at))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(at))
                .add(messages::Column::Id.lte(frontier.message_id)),
        )
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn to_row(model: messages::Model) -> MessageRow {
    MessageRow {
        id: model.id,
        request_id: model.request_id,
        role: model.role,
        content: model.content,
        model: model.model,
        input_tokens: model.input_tokens,
        output_tokens: model.output_tokens,
        created_at: from_db(model.created_at),
    }
}

#[async_trait]
impl<R: DBRunner> MessageRepository for R {
    async fn list_live_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRow>, DomainError> {
        let select = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope);
        let page = paginate_odata::<MessageFilterField, MessageODataMapper, _, _, _, _>(
            select,
            self,
            query,
            ("id", SortDir::Asc),
            limit_cfg(),
            to_row,
        )
        .await?;
        Ok(page)
    }

    async fn find_live_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        let model = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .and_id(id)?
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn recent_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<MessageRow>, DomainError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let mut select = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::RequestId.is_not_null())
            .filter(messages::Column::DeletedAt.is_null())
            .filter(messages::Column::IsCompressed.eq(false));
        if let Some(request_id) = query.excluded_request {
            select = select.filter(messages::Column::RequestId.ne(request_id));
        }
        // The frontier decides, not the flag: a message at or before it is part
        // of the summary even when it is not flagged.
        if let Some(frontier) = &query.after {
            select = select.filter(after_frontier(frontier));
        }
        let mut rows = select
            .secure()
            .scope_with(scope)
            .order_by(messages::Column::CreatedAt, Order::Desc)
            .order_by(messages::Column::Id, Order::Desc)
            .limit(query.limit)
            .all(self)
            .await?
            .into_iter()
            .map(to_row)
            .collect::<Vec<_>>();
        rows.reverse();
        Ok(rows)
    }

    async fn latest_assistant_tokens(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<u64>, DomainError> {
        let model = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::Role.eq("assistant"))
            .filter(messages::Column::DeletedAt.is_null())
            .filter(
                Condition::any()
                    .add(messages::Column::InputTokens.gt(0))
                    .add(messages::Column::OutputTokens.gt(0)),
            )
            .secure()
            .scope_with(scope)
            .order_by(messages::Column::CreatedAt, Order::Desc)
            .order_by(messages::Column::Id, Order::Desc)
            .one(self)
            .await?;
        Ok(model.map(|row| {
            u64::try_from(row.input_tokens)
                .unwrap_or_default()
                .saturating_add(u64::try_from(row.output_tokens).unwrap_or_default())
        }))
    }

    async fn insert_message(
        &self,
        scope: &AccessScope,
        message: NewMessage,
    ) -> Result<(), DomainError> {
        secure_insert::<messages::Entity>(
            messages::ActiveModel {
                id: Set(message.id),
                tenant_id: Set(message.tenant_id),
                chat_id: Set(message.chat_id),
                request_id: Set(Some(message.request_id)),
                role: Set(message.role.as_str().to_owned()),
                content: Set(message.content),
                content_type: Set("text".to_owned()),
                token_estimate: Set(0),
                provider_response_id: Set(message.provider_response_id),
                request_kind: Set("chat".to_owned()),
                features_used: Set(serde_json::json!([])),
                input_tokens: Set(message.input_tokens),
                output_tokens: Set(message.output_tokens),
                cache_read_input_tokens: Set(clamp_i64(message.breakdown.cache_read)),
                cache_write_input_tokens: Set(clamp_i64(message.breakdown.cache_write)),
                reasoning_tokens: Set(clamp_i64(message.breakdown.reasoning)),
                model: Set(message.model),
                is_compressed: Set(false),
                created_at: Set(to_db(message.created_at)),
                deleted_at: Set(None),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn attachment_summaries(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentSummaryRow>>, DomainError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let links = message_attachments::Entity::find()
            .filter(message_attachments::Column::ChatId.eq(chat_id))
            .filter(message_attachments::Column::MessageId.is_in(message_ids.iter().copied()))
            .secure()
            .scope_with(scope)
            .order_by(message_attachments::Column::CreatedAt, Order::Asc)
            .order_by(message_attachments::Column::AttachmentId, Order::Asc)
            .all(self)
            .await?;
        if links.is_empty() {
            return Ok(HashMap::new());
        }

        // Inner join with the live attachments of the same chat.
        let attachment_ids: Vec<Uuid> = links.iter().map(|link| link.attachment_id).collect();
        let live: HashMap<Uuid, attachments::Model> = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::Id.is_in(attachment_ids))
            .secure()
            .scope_with(scope)
            .all(self)
            .await?
            .into_iter()
            .map(|attachment| (attachment.id, attachment))
            .collect();

        let mut grouped: HashMap<Uuid, Vec<AttachmentSummaryRow>> = HashMap::new();
        for link in links {
            let Some(attachment) = live.get(&link.attachment_id) else {
                continue;
            };
            grouped
                .entry(link.message_id)
                .or_default()
                .push(AttachmentSummaryRow {
                    message_id: link.message_id,
                    attachment_id: attachment.id,
                    filename: attachment.filename.clone(),
                    kind: attachment.attachment_kind.clone(),
                    status: attachment.status.clone(),
                    thumbnail: attachment.img_thumbnail.clone(),
                    thumbnail_width: attachment.img_thumbnail_width,
                    thumbnail_height: attachment.img_thumbnail_height,
                });
        }
        Ok(grouped)
    }
}

/// The newest assistant message with a token count, optionally ignoring the
/// messages of one turn.
async fn newest_assistant_tokens<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    chat_id: Uuid,
    excluded_request: Option<Uuid>,
) -> Result<Option<u64>, DomainError> {
    let mut select = messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::Role.eq("assistant"))
        .filter(messages::Column::DeletedAt.is_null())
        .filter(
            Condition::any()
                .add(messages::Column::InputTokens.gt(0))
                .add(messages::Column::OutputTokens.gt(0)),
        );
    if let Some(request_id) = excluded_request {
        select = select.filter(
            Condition::any()
                .add(messages::Column::RequestId.is_null())
                .add(messages::Column::RequestId.ne(request_id)),
        );
    }
    let model = select
        .secure()
        .scope_with(scope)
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .one(runner)
        .await?;
    Ok(model.map(|row| {
        u64::try_from(row.input_tokens)
            .unwrap_or_default()
            .saturating_add(u64::try_from(row.output_tokens).unwrap_or_default())
    }))
}

#[async_trait]
impl<R: DBRunner> MessageMutationRepository for R {
    async fn find_turn_user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        let model = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::RequestId.eq(request_id))
            .filter(messages::Column::Role.eq("user"))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn soft_delete_turn_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        let updated = messages::Entity::update_many()
            .col_expr(messages::Column::DeletedAt, Expr::value(to_db(now)))
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::RequestId.eq(request_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected)
    }

    async fn clear_compression(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<u64, DomainError> {
        let updated = messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(false))
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::IsCompressed.eq(true))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected)
    }

    async fn latest_assistant_tokens_excluding(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<u64>, DomainError> {
        newest_assistant_tokens(self, scope, chat_id, Some(request_id)).await
    }

    async fn live_linked_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let links = message_attachments::Entity::find()
            .filter(message_attachments::Column::ChatId.eq(chat_id))
            .filter(message_attachments::Column::MessageId.eq(message_id))
            .secure()
            .scope_with(scope)
            .order_by(message_attachments::Column::CreatedAt, Order::Asc)
            .order_by(message_attachments::Column::AttachmentId, Order::Asc)
            .all(self)
            .await?;
        if links.is_empty() {
            return Ok(Vec::new());
        }
        // Inner join with the live attachments of the same chat.
        let live: std::collections::HashSet<Uuid> = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::Id.is_in(links.iter().map(|link| link.attachment_id)))
            .secure()
            .scope_with(scope)
            .all(self)
            .await?
            .into_iter()
            .map(|attachment| attachment.id)
            .collect();
        Ok(links
            .into_iter()
            .map(|link| link.attachment_id)
            .filter(|id| live.contains(id))
            .collect())
    }
}
