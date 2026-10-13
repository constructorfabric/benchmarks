//! `SeaORM` implementation of the message repository over Secure ORM.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Value};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::odata::sea_orm_filter::{
    FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata,
};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{attachments, message_attachments, message_reactions, messages};
use super::map_scope_err;
use crate::domain::error::DomainError;
use crate::domain::message::{
    AttachmentSummary, MessageFilterField, MessageRepository, MessageRole, MessageView, Reaction,
    ReactionView, Thumbnail,
};

/// Default and maximum page size of the message list.
const LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Message repository over the gear's database.
pub struct MessageRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
}

impl MessageRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>) -> Self {
        Self { db }
    }
}

/// Maps the message filter fields to `messages` columns and extracts cursor values from rows.
struct MessageODataMapper;

impl FieldToColumn<MessageFilterField> for MessageODataMapper {
    type Column = messages::Column;

    fn map_field(field: MessageFilterField) -> messages::Column {
        match field {
            MessageFilterField::CreatedAt => messages::Column::CreatedAt,
            MessageFilterField::Id => messages::Column::Id,
            MessageFilterField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageFilterField> for MessageODataMapper {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageFilterField) -> Value {
        match field {
            MessageFilterField::CreatedAt => {
                Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageFilterField::Id => Value::Uuid(Some(model.id)),
            MessageFilterField::Role => Value::String(Some(model.role.clone())),
        }
    }
}

/// The default order is chronological: `created_at asc`; `id asc` is added as the tiebreaker.
fn with_default_order(query: &ODataQuery) -> ODataQuery {
    if query.cursor.is_some() || !query.order.0.is_empty() {
        return query.clone();
    }
    let mut ordered = query.clone();
    ordered.order = ODataOrderBy(vec![OrderKey {
        field: "created_at".to_owned(),
        dir: SortDir::Asc,
    }]);
    ordered
}

fn to_summary(row: attachments::Model) -> AttachmentSummary {
    let ready_image = row.status == "ready" && row.attachment_kind == "image";
    let thumbnail = match (
        ready_image,
        row.img_thumbnail,
        row.img_thumbnail_width,
        row.img_thumbnail_height,
    ) {
        (true, Some(data), Some(width), Some(height)) => Some(Thumbnail {
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
            data,
        }),
        _ => None,
    };
    AttachmentSummary {
        attachment_id: row.id,
        kind: row.attachment_kind,
        filename: row.filename,
        status: row.status,
        thumbnail,
    }
}

#[async_trait]
impl MessageRepository for MessageRepositoryImpl {
    async fn list(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let conn = self.db.conn()?;
        let tenant_scope = scope.tenant_only();
        let select = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&tenant_scope);
        let page = paginate_odata::<MessageFilterField, MessageODataMapper, _, _, _, _>(
            select,
            &conn,
            &with_default_order(query),
            ("id", SortDir::Asc),
            LIST_LIMITS,
            |model| model,
        )
        .await?;

        let ids: Vec<Uuid> = page.items.iter().map(|message| message.id).collect();
        let mut attachments_by_message = self
            .attachments_of(&conn, &tenant_scope, chat_id, &ids)
            .await?;
        let mut reactions = self.reactions_of(&conn, scope, user_id, &ids).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for model in page.items {
            let role = MessageRole::parse(&model.role)
                .ok_or_else(|| DomainError::internal("a message has an unknown role"))?;
            let request_id = model
                .request_id
                .ok_or_else(|| DomainError::internal("a stored message has no request id"))?;
            items.push(MessageView {
                id: model.id,
                request_id,
                role,
                content: model.content,
                model: model.model,
                input_tokens: model.input_tokens,
                output_tokens: model.output_tokens,
                created_at: model.created_at,
                attachments: attachments_by_message.remove(&model.id).unwrap_or_default(),
                my_reaction: if role == MessageRole::Assistant {
                    reactions.remove(&model.id)
                } else {
                    None
                },
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn role_of(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRole>, DomainError> {
        let conn = self.db.conn()?;
        Ok(messages::Entity::find()
            .filter(messages::Column::Id.eq(message_id))
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .and_then(|model| MessageRole::parse(&model.role)))
    }

    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: Reaction,
        now: OffsetDateTime,
    ) -> Result<ReactionView, DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let updated = message_reactions::Entity::update_many()
                        .col_expr(
                            message_reactions::Column::Reaction,
                            Expr::value(reaction.as_str()),
                        )
                        .col_expr(message_reactions::Column::CreatedAt, Expr::value(now))
                        .filter(message_reactions::Column::MessageId.eq(message_id))
                        .filter(message_reactions::Column::UserId.eq(user_id))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await
                        .map_err(map_scope_err)?
                        .rows_affected;
                    if updated == 0 {
                        let row = message_reactions::ActiveModel {
                            id: ActiveValue::Set(Uuid::new_v4()),
                            message_id: ActiveValue::Set(message_id),
                            user_id: ActiveValue::Set(user_id),
                            tenant_id: ActiveValue::Set(tenant_id),
                            reaction: ActiveValue::Set(reaction.as_str().to_owned()),
                            created_at: ActiveValue::Set(now),
                        };
                        secure_insert::<message_reactions::Entity>(row, &scope, tx)
                            .await
                            .map_err(map_scope_err)?;
                    }
                    Ok(ReactionView {
                        message_id,
                        reaction,
                        created_at: now,
                    })
                })
            })
            .await
    }

    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        message_reactions::Entity::delete_many()
            .filter(message_reactions::Column::MessageId.eq(message_id))
            .filter(message_reactions::Column::UserId.eq(user_id))
            .secure()
            .scope_with(scope)
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(())
    }
}

impl MessageRepositoryImpl {
    /// The non-deleted attachments of the page's messages, keyed by message.
    async fn attachments_of(
        &self,
        conn: &impl toolkit_db::secure::DBRunner,
        tenant_scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentSummary>>, DomainError> {
        let mut by_message: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        if message_ids.is_empty() {
            return Ok(by_message);
        }
        let links = message_attachments::Entity::find()
            .filter(message_attachments::Column::ChatId.eq(chat_id))
            .filter(message_attachments::Column::MessageId.is_in(message_ids.to_vec()))
            .secure()
            .scope_with(tenant_scope)
            .all(conn)
            .await
            .map_err(map_scope_err)?;
        if links.is_empty() {
            return Ok(by_message);
        }
        let attachment_ids: Vec<Uuid> = links.iter().map(|link| link.attachment_id).collect();
        let rows: HashMap<Uuid, attachments::Model> = attachments::Entity::find()
            .filter(attachments::Column::Id.is_in(attachment_ids))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(tenant_scope)
            .all(conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        for link in links {
            if let Some(row) = rows.get(&link.attachment_id) {
                by_message
                    .entry(link.message_id)
                    .or_default()
                    .push(to_summary(row.clone()));
            }
        }
        for summaries in by_message.values_mut() {
            summaries.sort_by(|a, b| a.filename.cmp(&b.filename));
        }
        Ok(by_message)
    }

    /// The caller's reactions on the page's messages.
    async fn reactions_of(
        &self,
        conn: &impl toolkit_db::secure::DBRunner,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Reaction>, DomainError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        Ok(message_reactions::Entity::find()
            .filter(message_reactions::Column::MessageId.is_in(message_ids.to_vec()))
            .filter(message_reactions::Column::UserId.eq(user_id))
            .secure()
            .scope_with(scope)
            .all(conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .filter_map(|row| Some((row.message_id, Reaction::parse(&row.reaction)?)))
            .collect())
    }
}
