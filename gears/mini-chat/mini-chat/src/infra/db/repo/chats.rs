//! `ChatRepository` over the Secure ORM.

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::odata::paginate_odata;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::limit_cfg;
use super::odata::ChatODataMapper;
use crate::domain::chats::{ChatFilterField, ChatRow, NewChat};
use crate::domain::error::DomainError;
use crate::domain::ports::ChatRepository;
use crate::infra::db::entity::{attachments, chats, messages};
use crate::infra::db::timestamp::{from_db, to_db};

/// Stored title of an untitled chat. Titles are validated to 1..=255 characters, so
/// the empty string never collides with a real title; keeping the column free of
/// `NULL` lets `ORDER BY title` and the keyset cursor predicate agree on every
/// row (a `NULL` compares as neither greater nor smaller, and `SQLite` and
/// `PostgreSQL` sort it at opposite ends).
const NO_TITLE: &str = "";

fn to_row(model: chats::Model) -> ChatRow {
    ChatRow {
        id: model.id,
        tenant_id: model.tenant_id,
        user_id: model.user_id,
        model: model.model,
        // An untitled chat is stored as the empty title (see `NO_TITLE`).
        title: model.title.filter(|title| !title.is_empty()),
        is_temporary: model.is_temporary,
        created_at: from_db(model.created_at),
        updated_at: from_db(model.updated_at),
    }
}

#[derive(FromQueryResult)]
struct MessageCount {
    chat_id: Uuid,
    cnt: i64,
}

#[async_trait]
impl<R: DBRunner> ChatRepository for R {
    async fn find_live_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<ChatRow>, DomainError> {
        let model = chats::Entity::find()
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .and_id(id)?
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn list_live_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatRow>, DomainError> {
        let select = chats::Entity::find()
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope);
        let page = paginate_odata::<ChatFilterField, ChatODataMapper, _, _, _, _>(
            select,
            self,
            query,
            ("id", SortDir::Desc),
            limit_cfg(),
            to_row,
        )
        .await?;
        Ok(page)
    }

    async fn count_live_messages(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, DomainError> {
        if chat_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<MessageCount> = messages::Entity::find()
            .filter(messages::Column::ChatId.is_in(chat_ids.iter().copied()))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .project_all(self, |select| {
                select
                    .select_only()
                    .column(messages::Column::ChatId)
                    .column_as(Expr::col(messages::Column::Id).count(), "cnt")
                    .group_by(messages::Column::ChatId)
                    .into_model::<MessageCount>()
            })
            .await?;
        Ok(rows.into_iter().map(|row| (row.chat_id, row.cnt)).collect())
    }

    async fn insert_chat(
        &self,
        scope: &AccessScope,
        chat: NewChat,
    ) -> Result<ChatRow, DomainError> {
        let model = secure_insert::<chats::Entity>(
            chats::ActiveModel {
                id: Set(chat.id),
                tenant_id: Set(chat.tenant_id),
                user_id: Set(chat.user_id),
                model: Set(chat.model),
                title: Set(Some(chat.title.unwrap_or_else(|| NO_TITLE.to_owned()))),
                is_temporary: Set(false),
                created_at: Set(to_db(chat.now)),
                updated_at: Set(to_db(chat.now)),
                deleted_at: Set(None),
            },
            scope,
            self,
        )
        .await?;
        Ok(to_row(model))
    }

    async fn update_chat_title(
        &self,
        scope: &AccessScope,
        id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRow>, DomainError> {
        let updated = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title.to_owned()))
            .col_expr(chats::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chats::Column::Id.eq(id))
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec_with_returning(self)
            .await?;
        Ok(updated.into_iter().next().map(to_row))
    }

    async fn soft_delete_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRow>, DomainError> {
        let deleted = chats::Entity::update_many()
            .col_expr(chats::Column::DeletedAt, Expr::value(to_db(now)))
            .col_expr(chats::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chats::Column::Id.eq(id))
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec_with_returning(self)
            .await?;
        Ok(deleted.into_iter().next().map(to_row))
    }

    async fn touch_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = chats::Entity::update_many()
            .col_expr(chats::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chats::Column::Id.eq(id))
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected > 0)
    }

    async fn lock_live_chat(&self, scope: &AccessScope, id: Uuid) -> Result<bool, DomainError> {
        // A no-op write: it takes the row lock (PostgreSQL) / the write lock
        // (SQLite) that serializes the per-chat upload limits, and it refuses a
        // chat that was deleted meanwhile.
        let locked = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::col(chats::Column::Title))
            .filter(chats::Column::Id.eq(id))
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(locked.rows_affected > 0)
    }

    async fn chat_is_deleted(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        let model = chats::Entity::find()
            .filter(chats::Column::Id.eq(id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(|chat| chat.deleted_at.is_some()))
    }

    async fn mark_attachments_for_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<u64, DomainError> {
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::CleanupStatus, Expr::value("pending"))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected)
    }
}
